//! Drives `omni search` end to end against synthetic Claude and Codex stores.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_ID: &str = "019f0000-0000-7000-8000-000000000001";
const OTHER_CODEX_ID: &str = "019f0000-0000-7000-8000-000000000002";
const CLAUDE_TITLE: &str = "Fix pagination cursor handling";
const SECRET: &str = "sk-proj-SYNTHETICSECRET0123456789";

#[test]
fn a_missing_current_rollout_revokes_only_its_cached_session_after_incomplete_discovery() {
    let source = format!("codex:{CODEX_ID}");
    for command in [
        &["index", "--provider", "codex"][..],
        &["show", &source],
        &["show", CODEX_ID],
        &["search", "Pagination", "--provider", "codex"],
        &["search", "Pagination", "--no-index", "--provider", "codex"],
    ] {
        check_missing_current_rollout(command);
    }
}

fn check_missing_current_rollout(command: &[&str]) {
    let fixture = Fixture::new();
    let codex = fixture.root.join("codex");
    let day = codex.join("sessions/2026/01/02");
    let original = day.join(format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"));
    let peer = day.join(format!(
        "rollout-2026-01-02T00-00-00-{OTHER_CODEX_ID}.jsonl"
    ));
    let generation = "019f0000-0000-7000-8000-000000000093";
    let current = day.join(format!(
        "rollout-2026-01-02T00-00-00-{CODEX_ID}_{generation}.jsonl"
    ));
    let db = rusqlite::Connection::open(codex.join("state_5.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES (?1, ?2, 'legacy')",
        rusqlite::params![CODEX_ID, original.to_str().unwrap()],
    )
    .unwrap();
    fixture.index_codex();
    let results = |query, mode| {
        result_sessions(&fixture.search_json(&[
            query,
            mode,
            "--all-projects",
            "--provider",
            "codex",
        ]))
    };
    assert_eq!(
        results("Pagination", "--cached"),
        [format!("codex:{CODEX_ID}")]
    );

    fs::remove_file(&original).unwrap();
    fs::remove_file(peer).unwrap();
    let malformed =
        day.join("rollout-2026-01-02T00-00-00-019f0000-0000-7000-8000-000000000094.jsonl");
    fs::write(malformed, "{incomplete unrelated header").unwrap();
    db.execute(
        "UPDATE threads SET rollout_path=?1, history_mode='paginated' WHERE id=?2",
        rusqlite::params![current.to_str().unwrap(), CODEX_ID],
    )
    .unwrap();
    let output = fixture
        .command()
        .arg("--json")
        .args(command)
        .output()
        .unwrap();
    assert_eq!(output.status.success(), command[0] != "show", "{command:?}");
    if command[0] == "search" {
        assert!(result_sessions(&serde_json::from_slice(&output.stdout).unwrap()).is_empty());
    }
    assert!(results("Pagination", "--cached").is_empty());
    assert_eq!(
        results("zebracorn", "--cached"),
        [format!("codex:{OTHER_CODEX_ID}")]
    );

    write_jsonl(
        &current,
        &[
            json!({"type":"session_meta","ordinal":0,"payload":{"id":CODEX_ID,"session_id":CODEX_ID,
              "history_mode":"paginated","cwd":fixture.workspace}}),
            json!({"type":"event_msg","ordinal":1,"payload":{"type":"item_completed","thread_id":CODEX_ID,
              "turn_id":"restored","item":{"type":"AgentMessage","id":"answer","content":[
                  {"type":"Text","text":"Restored cinnabar response"}]}}}),
            json!({"type":"response_item","ordinal":2,"payload":{"type":"message","role":"assistant",
              "content":[{"type":"output_text","text":"Restored cinnabar response"}]}}),
        ],
    );
    assert_eq!(fixture.index_codex()["indexed"], 1);
    assert_eq!(
        results("cinnabar", "--cached"),
        [format!("codex:{CODEX_ID}")]
    );
    assert!(results("Pagination", "--cached").is_empty());
    assert_eq!(fixture.index_codex()["stale"], 0);
}

#[test]
fn a_preferred_rollout_replaces_warm_search_even_with_the_same_mtime() {
    for (direct_read, seconds_older) in [(false, 0), (true, 0), (true, 60)] {
        let fixture = Fixture::new();
        let name = format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl");
        let active = fixture.root.join("codex/sessions/2026/01/02").join(&name);
        let archive = fixture.root.join("codex/archived_sessions");
        fs::create_dir(&archive).unwrap();
        let archived = archive.join(&name);
        fs::rename(&active, &archived).unwrap();
        fixture.index_codex();
        let restored_workspace = fixture.root.join("restored-workspace");
        fs::create_dir(&restored_workspace).unwrap();
        write_codex_session(
            &fixture.root,
            CODEX_ID,
            &restored_workspace,
            "Restored session",
            "Cinnabar from the active copy",
        );
        fs::File::options()
            .write(true)
            .open(&active)
            .unwrap()
            .set_modified(
                archived.metadata().unwrap().modified().unwrap()
                    - std::time::Duration::from_secs(seconds_older),
            )
            .unwrap();
        if direct_read {
            let show = fixture
                .command()
                .args(["show", &format!("codex:{CODEX_ID}")])
                .output()
                .unwrap();
            assert!(
                show.status.success(),
                "{}",
                String::from_utf8_lossy(&show.stderr)
            );
        } else {
            assert_eq!(fixture.index_codex()["indexed"], 1);
        }
        for cached in [true, false] {
            for (workspace, restored) in [(&fixture.workspace, false), (&restored_workspace, true)]
            {
                let mut args = vec![
                    "Cinnabar",
                    "--show-text",
                    "--provider",
                    "codex",
                    "--project",
                    workspace.to_str().unwrap(),
                ];
                if cached {
                    args.push("--cached");
                }
                let hits = fixture.search_json(&args);
                if restored {
                    assert_eq!(result_sessions(&hits), [format!("codex:{CODEX_ID}")]);
                    assert_eq!(hits["results"][0]["title"], "Restored session");
                    assert_eq!(
                        hits["results"][0]["project_path"],
                        restored_workspace.to_str().unwrap()
                    );
                } else {
                    assert!(result_sessions(&hits).is_empty());
                }
            }
        }
        assert!(
            result_sessions(&fixture.search_json(&[
                "Pagination",
                "--cached",
                "--provider",
                "codex"
            ]))
            .is_empty()
        );
        assert_eq!(fixture.index_codex()["indexed"], 0);
    }
}

#[test]
fn index_saves_the_workspace_from_a_large_rollback_while_search_text_stays_sampled() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    let fixture = Fixture::new();
    let moved = fixture.root.join("other-workspace");
    let path = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    fs::remove_file(&path).unwrap();
    let mut writer = GzEncoder::new(
        fs::File::create(path.with_extension("jsonl.gz")).unwrap(),
        Compression::fast(),
    );
    let user =
        |text: &str| json!({"type":"event_msg","payload":{"type":"user_message","message":text}});
    let context = |cwd: &Path| json!({"type":"turn_context","payload":{"cwd":cwd}});
    for record in [
        json!({"type":"session_meta","payload":{"id":CODEX_ID,"cwd":fixture.workspace}}),
        context(&fixture.workspace),
        user("opening request"),
        context(&moved),
        user("second turn"),
        json!({"type":"event_msg","payload":{"type":"agent_message","message":"Amethystlarch surviving reply"}}),
    ] {
        writeln!(writer, "{record}").unwrap();
    }
    let padding = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"fixture","output":"a".repeat(64_000)}}).to_string();
    for _ in 0..268 {
        writeln!(writer, "{padding}").unwrap();
    }
    for record in [
        context(&fixture.workspace),
        user("temporary third turn"),
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
    ] {
        writeln!(writer, "{record}").unwrap();
    }
    writer.finish().unwrap();
    fixture.index_codex();
    for cached in [false, true] {
        for (project, expected) in [(&fixture.workspace, false), (&moved, true)] {
            let mut args = vec![
                "Amethystlarch",
                "--show-text",
                "--provider",
                "codex",
                "--project",
                project.to_str().unwrap(),
            ];
            if cached {
                args.push("--cached");
            }
            let results = fixture.search_json(&args);
            assert_eq!(
                result_sessions(&results).contains(&format!("codex:{CODEX_ID}")),
                expected,
                "{results}"
            );
            if expected {
                assert_eq!(results["results"][0]["coverage"], "preview");
                assert_eq!(
                    results["results"][0]["project_path"],
                    moved.to_str().unwrap()
                );
            }
        }
    }
    assert_eq!(fixture.index_codex()["indexed"], 0);
}

#[test]
fn cold_scoped_search_uses_the_surviving_codex_workspace_before_ranking() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    for (from_creation, query, damaged) in [
        (true, "plumworkspacereply", false),
        (false, "plumworkspacereply", false),
        (false, "plumworkspacereply", true),
        (true, "saffroncreationbranch", false),
    ] {
        let fixture = Fixture::new();
        let moved = fixture.root.join("other-workspace");
        let path = fixture.root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        fs::remove_file(&path).unwrap();
        let mut writer = GzEncoder::new(
            fs::File::create(path.with_extension("jsonl.gz")).unwrap(),
            Compression::fast(),
        );
        let header = json!({"type":"session_meta","payload":{"id":CODEX_ID,"cwd":fixture.workspace,"git":{"branch":"saffroncreationbranch"}}});
        writeln!(writer, "{header}").unwrap();
        if damaged {
            writer
                .write_all(b"{\"type\":\"turn_context\",\"payload\":\n")
                .unwrap();
        }
        for record in [
            json!({"type":"turn_context","payload":{"cwd":moved}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Find my active request"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"plumworkspacereply"}]}}),
        ] {
            writeln!(writer, "{record}").unwrap();
        }
        if damaged {
            for discarded in [
                json!({"type":"turn_context","payload":{"cwd":fixture.workspace}}),
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Discard this turn"}]}}),
                json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
            ] {
                writeln!(writer, "{discarded}").unwrap();
            }
        }
        writer.finish().unwrap();
        let project = if from_creation {
            &fixture.workspace
        } else {
            &moved
        };
        let search = |cwd: &Path, term: &str, extra: &[&str]| {
            let output = fixture
                .command()
                .current_dir(cwd)
                .args(["--json", "search", "--provider", "codex", term])
                .args(extra)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        assert!(
            result_sessions(&search(project, "plumworkspacereply", &["--no-index"])).is_empty()
        );
        let live = search(project, query, &[]);
        let session = format!("codex:{CODEX_ID}");
        assert_eq!(
            result_sessions(&live).contains(&session),
            !from_creation,
            "{live}"
        );
        if !from_creation {
            assert_eq!(live["results"][0]["project_path"], moved.to_str().unwrap());
        }
        for (cwd, expected) in [(&fixture.workspace, false), (&moved, true)] {
            let hits = search(
                &fixture.workspace,
                "plumworkspacereply",
                &["--project", cwd.to_str().unwrap(), "--no-index"],
            );
            assert_eq!(
                result_sessions(&hits).contains(&session),
                expected,
                "{hits}"
            );
        }
    }
}

#[test]
fn scoped_search_can_recheck_a_stamped_full_document_beyond_the_cold_probe_limit() {
    use std::io::Write;

    let fixture = Fixture::new();
    let project = fixture.root.join("other-workspace");
    let path = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    let mut file = fs::File::options().append(true).open(path).unwrap();
    let padding =
        json!({"type":"event_msg","payload":{"type":"token_count","padding":"x".repeat(8192)}});
    for _ in 0..600 {
        writeln!(file, "{padding}").unwrap();
    }
    writeln!(
        file,
        "{}",
        json!({"type":"turn_context","payload":{"cwd":project}})
    )
    .unwrap();
    drop(file);
    let session = format!("codex:{CODEX_ID}");
    assert!(
        fixture
            .command()
            .args(["--json", "show", &session])
            .output()
            .unwrap()
            .status
            .success()
    );
    let db = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
    for valid_stamp in [true, false] {
        // Recreate the version-5 catalog that preceded the verified-workspace table.
        assert_eq!(
            db.execute(
                "UPDATE session_trajectories SET document_version=5 WHERE provider='codex' AND session_id=?1 AND source_complete=1",
                [CODEX_ID],
            ).unwrap(),
            1
        );
        db.execute("DROP TABLE codex_indexed_workspaces", [])
            .unwrap();
        if !valid_stamp {
            db.execute("UPDATE session_index SET source_contents_id=randomblob(16) WHERE provider='codex' AND session_id=?1",[CODEX_ID]).unwrap();
        }
        let hits = fixture.search_json(&[
            "Pagination",
            "--provider",
            "codex",
            "--project",
            project.to_str().unwrap(),
        ]);
        assert_eq!(
            result_sessions(&hits).contains(&session),
            valid_stamp,
            "{hits}"
        );
        if !valid_stamp {
            assert!(
                hits["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|warning| warning.as_str().unwrap().contains("could not check"))
            );
        }
    }
}

#[test]
fn full_reads_keep_the_latest_turn_workspace_across_header_only_scans() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    let fixture = Fixture::new();
    let name = format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl");
    let source = fixture.root.join("codex/sessions/2026/01/02").join(&name);
    let archive = fixture.root.join("codex/archived_sessions");
    fs::create_dir(&archive).unwrap();
    let archived = archive.join(name);
    fs::rename(&source, &archived).unwrap();
    let original = fs::read(&archived).unwrap();
    fixture.index_codex();

    let active = source.with_extension("jsonl.gz");
    let restored = fixture.root.join("restored-workspace");
    let replacement = fixture.root.join("replacement-workspace");
    fs::create_dir(&restored).unwrap();
    fs::create_dir(&replacement).unwrap();
    let session = format!("codex:{CODEX_ID}");
    let run_json = |args: &[&str]| {
        let output = fixture.command().arg("--json").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };

    for (workspace, marker) in [
        (&restored, "cinnabarreply"),
        (&replacement, "turquoisereply"),
    ] {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        for row in [
            json!({"type":"session_meta","payload":{"id":CODEX_ID,"cwd":fixture.workspace,
                "git":{"branch":"creation-branch"}}}),
            json!({"type":"turn_context","payload":{"cwd":workspace}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":marker}]}}),
        ] {
            writeln!(encoder, "{row}").unwrap();
        }
        let bytes = encoder.finish().unwrap();
        let mut staged = tempfile::NamedTempFile::new_in(active.parent().unwrap()).unwrap();
        staged.write_all(&bytes).unwrap();
        staged.persist(&active).unwrap();

        if workspace == &restored {
            let shown = run_json(&["show", &session]);
            assert_eq!(
                shown["workspace"]["current_dir"],
                workspace.to_str().unwrap()
            );
            assert!(shown["workspace"]["git"]["branch"].is_null());
        } else {
            let listed = run_json(&[
                "list",
                "--provider",
                "codex",
                "--project",
                restored.to_str().unwrap(),
            ]);
            assert!(listed["sessions"].as_array().unwrap().is_empty());
            assert_eq!(fixture.index_codex()["indexed"], 1);
        }

        let index = fixture.index_codex();
        assert_eq!(index["stale"], 0);
        assert_eq!(index["indexed"], 0);
        for project in [&fixture.workspace, &restored, &replacement] {
            let expected = project == workspace;
            let project = project.to_str().unwrap();
            let listed = run_json(&["list", "--provider", "codex", "--project", project]);
            let row = listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["session"]["id"] == CODEX_ID);
            assert_eq!(row.is_some(), expected, "list in {project}");
            if let Some(row) = row {
                assert!(row["git_branch"].is_null());
            }
            for mode in ["--no-index", "--cached"] {
                let hits = fixture.search_json(&[
                    marker,
                    "--provider",
                    "codex",
                    "--project",
                    project,
                    mode,
                ]);
                assert_eq!(
                    result_sessions(&hits).contains(&session),
                    expected,
                    "{mode} search in {project}"
                );
            }
        }
        assert_eq!(fs::read(&active).unwrap(), bytes);
        assert_eq!(fs::read(&archived).unwrap(), original);
    }
}

#[test]
fn missing_rollout_names_follow_the_last_readable_assignment_in_their_home() {
    let name = json!({"id":CODEX_ID,"thread_name":"Violet Quokka"}).to_string();
    let renamed = json!({"id":CODEX_ID,"thread_name":"Copper Fennel"}).to_string();
    for (absent_index, expected, partial, search) in [
        (String::new(), None, false, false),
        (String::new(), None, false, true),
        (String::new(), None, true, false),
        (
            format!("{{\n{renamed}\n"),
            Some("Copper Fennel"),
            false,
            false,
        ),
        (
            format!("{{\n{renamed}\n"),
            Some("Copper Fennel"),
            false,
            true,
        ),
        ("{".into(), Some("Violet Quokka"), false, false),
    ] {
        let fixture = Fixture::new();
        let codex = fixture.root.join("codex");
        let parked = fixture.root.join("parked");
        let inner = codex.join("sessions/nested");
        fs::rename(codex.join("sessions"), &parked).unwrap();
        fs::create_dir_all(&inner).unwrap();
        fs::rename(&parked, inner.join("sessions")).unwrap();
        let sidecar = codex.join("session_index.jsonl");
        fs::write(&sidecar, &name).unwrap();
        fixture.index_codex();
        let args = ["Violet Quokka", "--provider", "codex"];
        assert!(!result_sessions(&fixture.search_json(&args)).is_empty());

        fs::write(&sidecar, "{").unwrap();
        let switched = fixture
            .command()
            .env("CODEX_HOME", &inner)
            .args(["--json", "search"])
            .args(args)
            .output()
            .unwrap();
        assert!(switched.status.success());
        assert!(result_sessions(&serde_json::from_slice(&switched.stdout).unwrap()).is_empty());
        let unreadable = codex.join("sessions/unreadable.jsonl");
        if partial {
            fs::write(&unreadable, "{").unwrap();
        }
        fs::rename(&inner, &parked).unwrap();
        fs::write(&sidecar, absent_index).unwrap();
        if search {
            fixture.search_json(&["Violet Quokka", "--provider", "codex", "--all-projects"]);
        } else {
            fixture.index_codex();
        }
        fs::write(&sidecar, "{").unwrap();
        fs::rename(&parked, &inner).unwrap();
        if partial {
            fs::remove_file(unreadable).unwrap();
        }
        for query in ["Violet Quokka", "Copper Fennel"] {
            let flags = [query, "--provider", "codex"];
            for flags in [flags.to_vec(), [&flags[..], &["--cached"]].concat()] {
                assert_eq!(
                    !result_sessions(&fixture.search_json(&flags)).is_empty(),
                    Some(query) == expected
                );
            }
        }
    }
}

#[test]
fn show_preserves_a_verified_name_only_for_the_same_rollout() {
    use omnis_ir::{Provider, SessionRef};

    let fixture = Fixture::new();
    let codex = fixture.root.join("codex");
    let sidecar = codex.join("session_index.jsonl");
    let session = SessionRef::new(Provider::Codex, CODEX_ID);
    let name = json!({"id": CODEX_ID, "thread_name": "Violet Quokka"});
    fs::write(&sidecar, name.to_string()).unwrap();
    fixture.index_codex();
    let check = |official: Option<&str>| {
        let output = fixture
            .command()
            .args(["--json", "show", &session.to_string()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(shown["title"].as_str(), official);
    };
    let opener_search = |expected: &str| {
        let hit =
            fixture.search_json(&["limter", "--provider", "codex", "--no-index", "--show-text"]);
        assert_eq!(hit["results"][0]["match"], "metadata", "{hit}");
        assert_eq!(hit["results"][0]["title"], expected, "{hit}");
    };
    opener_search("Violet Quokka");
    let ambiguous = format!(
        "{}\n{{",
        json!({"id": CODEX_ID, "thread_name": "Amber Heron"})
    );
    fs::write(&sidecar, &ambiguous).unwrap();
    check(Some("Violet Quokka"));
    fs::write(&sidecar, "\n").unwrap();
    opener_search("Tune the rate limiter backoff");
    check(None);

    fs::write(&sidecar, name.to_string()).unwrap();
    check(Some("Violet Quokka"));
    fs::write(&sidecar, ambiguous).unwrap();
    let file = codex.join(format!(
        "sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    let replacement = file.with_extension("new");
    fs::copy(&file, &replacement).unwrap();
    fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(fs::metadata(&file).unwrap().modified().unwrap())
        .unwrap();
    fs::rename(replacement, file).unwrap();
    check(None);
}

#[test]
fn show_repairs_stale_text_even_if_the_source_stamp_matches() {
    use omnis_ir::{Provider, SessionRef};
    use omnis_store::{SessionTrajectoryOrigin, Store, TrajectoryDocument};

    let fixture = Fixture::new();
    fixture.add_missing_ancestor();
    fixture.index_codex();
    let session = SessionRef::new(Provider::Codex, CODEX_ID);
    let store = Store::open(fixture.root.join("state/store.sqlite3")).unwrap();
    let state = store.trajectory_index_state(&session).unwrap().unwrap();
    let obsolete = "Superseded fixture content";
    store
        .upsert_trajectory_document(
            &session,
            &TrajectoryDocument {
                redacted_text: obsolete,
                projection: None,
                source_updated_at: state.source_updated_at,
                source_revision: state.source_revision,
                source_byte_count: obsolete.len(),
                indexed_byte_count: obsolete.len(),
                truncation_strategy: "none",
                source_complete: true,
                origin: SessionTrajectoryOrigin::Native,
                document_version: state.document_version,
                provider_limited: false,
                derived_title: None,
                withdrawn_title: None,
                title_is_verified: false,
                title_authoritative: true,
                local_opener: omnis_store::LocalOpenerEvidence::Unknown,
            },
        )
        .unwrap();
    assert_eq!(
        result_sessions(&fixture.search_json(&["Superseded", "--cached", "--all-projects"])),
        [session.to_string()]
    );
    assert!(
        fixture
            .command()
            .args(["show", &session.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        result_sessions(&fixture.search_json(&["Superseded", "--cached", "--all-projects"]))
            .is_empty()
    );
    assert_eq!(
        result_sessions(&fixture.search_json(&["unchanged", "--cached", "--all-projects"])),
        [session.to_string()]
    );
}

#[test]
fn rolled_back_local_opener_is_removed_from_a_paginated_sessions_search_title() {
    use std::io::Write;
    let reference = format!("codex:{CODEX_ID}");
    for show_first in [false, true] {
        let fixture = Fixture::new();
        fixture.add_missing_ancestor();
        fixture.index_codex();
        let search = |word, cached| {
            let mut args = vec!["--provider", "codex", "--show-text", "--all-projects", word];
            if cached {
                args.push("--cached");
            }
            fixture.search_json(&args)
        };
        assert_eq!(
            result_sessions(&search("backoff", true)),
            [reference.as_str()]
        );
        let path = fixture.root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}})
        )
        .unwrap();
        let original = fs::read(&path).unwrap();
        if show_first {
            let shown = fixture
                .command()
                .args(["show", &reference])
                .output()
                .unwrap();
            assert!(
                shown.status.success(),
                "{}",
                String::from_utf8_lossy(&shown.stderr)
            );
            assert!(!String::from_utf8_lossy(&shown.stdout).contains("backoff"));
        } else {
            assert!(result_sessions(&search("backoff", false)).is_empty());
        }
        assert!(result_sessions(&search("backoff", true)).is_empty());
        assert!(search(CODEX_ID, true)["results"][0]["title"].is_null());
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn sampled_fork_index_withdraws_a_legacy_title_after_rollback() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    let initial_title = "Tune the rate limiter backoff";
    for (gzip, repeated_later) in [(false, false), (true, false), (false, true)] {
        let fixture = Fixture::new();
        fixture.add_missing_ancestor();
        let plain = fixture.root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        let mut source = fs::read(&plain).unwrap();
        let pad = format!(
            "{{\"type\":\"turn_context\",\"payload\":{{}}}}{}\n",
            " ".repeat(1024 * 1024)
        );
        for _ in 0..16 {
            source.extend_from_slice(pad.as_bytes());
        }
        fs::remove_file(&plain).unwrap();
        let path = if gzip {
            plain.with_extension("jsonl.gz")
        } else {
            plain
        };
        let append = |bytes: &[u8]| {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            if gzip {
                let mut encoder = GzEncoder::new(file, Compression::fast());
                encoder.write_all(bytes).unwrap();
                encoder.finish().unwrap();
            } else {
                file.write_all(bytes).unwrap();
            }
        };
        append(&source);
        let search = |query: &str| {
            fixture.search_json(&[
                "--provider",
                "codex",
                "--show-text",
                "--cached",
                "--all-projects",
                query,
            ])
        };
        fixture.index_codex();
        assert!(search(CODEX_ID)["results"][0]["title"].is_null());
        // Older indexers used the first local turn as a title when the parent was missing.
        let db = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
        assert_eq!(
            db.execute(
                "UPDATE session_trajectories SET derived_title = ?1
                 WHERE provider = 'codex' AND session_id = ?2
                   AND derived_title IS NULL AND NOT derived_title_is_verified",
                [initial_title, CODEX_ID],
            )
            .unwrap(),
            1
        );
        drop(db);
        assert_eq!(search(CODEX_ID)["results"][0]["title"], initial_title);

        append(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_back\",\"num_turns\":1}}\n");
        if repeated_later {
            let new_opener = format!("New opener {}", "x".repeat(5 * 1024 * 1024));
            for text in [new_opener.as_str(), initial_title] {
                let row = json!({"type":"response_item","payload":{"type":"message","role":"user",
                    "content":[{"type":"input_text","text":text}]}});
                append(format!("{row}\n").as_bytes());
            }
        }
        let original = fs::read(&path).unwrap();
        let live = fixture.search_json(&[
            "--provider",
            "codex",
            "--show-text",
            "--all-projects",
            "backoff",
        ]);
        for results in [live, search("backoff")] {
            if repeated_later {
                assert_eq!(results["results"][0]["match"], "conversation");
                assert_eq!(results["results"][0]["coverage"], "preview");
            } else {
                assert!(result_sessions(&results).is_empty());
            }
        }
        assert!(search(CODEX_ID)["results"][0]["title"].is_null());
        assert_eq!(fixture.index_codex()["indexed"], 0);
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn sampled_ordinary_rollback_replaces_a_previously_verified_title() {
    assert_sampled_rollback_title(false);
}

#[test]
fn sampled_legacy_rollback_repairs_a_title_cached_before_its_mode_was_recognized() {
    assert_sampled_rollback_title(true);
}

fn assert_sampled_rollback_title(old_legacy_cache: bool) {
    use std::io::{BufWriter, Write};

    let fixture = Fixture::new();
    let path = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    if old_legacy_cache {
        let mut rows: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        rows[0]["payload"]["history_mode"] = json!("legacy");
        write_jsonl(&path, &rows);
    }
    let cached = |query: &str| {
        fixture.search_json(&[query, "--provider", "codex", "--show-text", "--cached"])
    };
    fixture.index_codex();
    assert_eq!(
        cached(CODEX_ID)["results"][0]["title"],
        "Tune the rate limiter backoff"
    );
    let mut file = BufWriter::new(fs::OpenOptions::new().append(true).open(&path).unwrap());
    let pad = format!(
        "{{\"type\":\"turn_context\",\"payload\":{{}}}}{}",
        " ".repeat(1024 * 1024)
    );
    for _ in 0..16 {
        writeln!(file, "{pad}").unwrap();
    }
    for row in [
        json!({"type":"dynamic_tools_updated","payload":{"thread_id":CODEX_ID,"tools":[]}}),
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"Investigate latency instead"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"Queue delay explains the slowdown"}]}}),
    ] {
        writeln!(file, "{row}").unwrap();
    }
    drop(file.into_inner().unwrap());
    let original = fs::read(&path).unwrap();
    if old_legacy_cache {
        fixture.index_codex();
        let db = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
        db.execute_batch(
            "DROP TABLE codex_workspace_rechecks;
             CREATE TABLE codex_workspace_rechecks(
                 trajectory_id INTEGER PRIMARY KEY REFERENCES session_trajectories(id) ON DELETE CASCADE,
                 source_file_id BLOB NOT NULL,source_contents_id BLOB NOT NULL);",
        ).unwrap();
        assert_eq!(
            db.execute(
                "UPDATE session_trajectories SET document_version=6, derived_title='Tune the rate limiter backoff',
                 derived_title_is_verified=1,derived_title_is_local_opener=0
                 WHERE provider='codex' AND session_id=?1 AND source_complete=0",
                [CODEX_ID],
            )
            .unwrap(),
            1,
        );
    }

    if old_legacy_cache {
        let pending = fixture.search_json(&["slowdown", "--provider", "codex"]);
        assert!(result_sessions(&pending).is_empty());
        assert!(
            pending["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| {
                    warning
                        .as_str()
                        .unwrap()
                        .contains("omni index --provider codex")
                }),
            "{pending}"
        );
        fixture.index_codex();
    }
    let live = fixture.search_json(&["backoff", "--provider", "codex", "--show-text"]);
    assert!(result_sessions(&live).is_empty());
    assert!(result_sessions(&cached("backoff")).is_empty());
    let retained = cached("slowdown");
    let scoped = fixture.search_json(&["slowdown", "--provider", "codex", "--show-text"]);
    assert_eq!(scoped["results"][0]["title"], "Investigate latency instead");
    assert_eq!(
        retained["results"][0]["title"],
        "Investigate latency instead"
    );
    assert_eq!(retained["results"][0]["coverage"], "preview");
    assert_eq!(fixture.index_codex()["stale"], 0);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn malformed_catalog_keeps_a_provider_name_after_a_local_rollback() {
    use std::io::Write;

    let fixture = Fixture::new();
    let codex = fixture.root.join("codex");
    let catalog = codex.join("session_index.jsonl");
    let name = "Violet Quokka";
    write_jsonl(&catalog, &[json!({"id":CODEX_ID,"thread_name":name})]);
    let cached = |query: &str| {
        fixture.search_json(&[query, "--provider", "codex", "--show-text", "--cached"])
    };
    fixture.index_codex();
    assert_eq!(cached(CODEX_ID)["results"][0]["title"], name);

    fs::write(&catalog, "{").unwrap();
    let path = codex.join(format!(
        "sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in [
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"Investigate Copperwren"}}),
    ] {
        writeln!(file, "{row}").unwrap();
    }
    drop(file);
    fixture.index_codex();
    let updated = cached("Copperwren");
    assert_eq!(updated["results"][0]["title"], name);
    assert_eq!(updated["results"][0]["match"], "conversation");
    assert!(result_sessions(&cached("backoff")).is_empty());
}

#[test]
fn an_incomplete_name_index_preserves_a_provider_name_but_updates_an_inferred_fork_title() {
    for provider_name in [Some("Violet Quokka"), None] {
        let fixture = Fixture::new();
        let codex = fixture.root.join("codex");
        let catalog = codex.join("session_index.jsonl");
        if let Some(name) = provider_name {
            write_jsonl(&catalog, &[json!({"id":CODEX_ID,"thread_name":name})]);
        }
        let path = codex.join(format!(
            "sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        let write_fork = |parent, opening, reply| {
            write_jsonl(
                &path,
                &[
                    json!({"type":"session_meta","payload":{"id":CODEX_ID,"cwd":fixture.workspace,"forked_from_id":parent}}),
                    json!({"type":"event_msg","payload":{"type":"user_message","message":opening}}),
                    json!({"type":"event_msg","payload":{"type":"agent_message","message":reply}}),
                ],
            );
        };
        let cached_title = |query| {
            fixture.search_json(&[query, "--provider", "codex", "--show-text", "--cached"])
                ["results"][0]["title"].clone()
        };
        write_fork(OTHER_CODEX_ID, "Opening copied request", "Initial answer");
        fixture.index_codex();
        assert_eq!(
            cached_title(CODEX_ID),
            provider_name.unwrap_or("Opening copied request")
        );

        fs::write(&catalog, "{").unwrap();
        write_fork(
            OTHER_CODEX_ID,
            "Updated copied request",
            "Copperwren answer",
        );
        fixture.index_codex();
        assert_eq!(
            cached_title("Copperwren"),
            provider_name.unwrap_or("Updated copied request")
        );

        if provider_name.is_some() {
            write_fork(
                "019ccd00-0000-7000-8000-000000000778",
                "Updated copied request",
                "Copperwren answer",
            );
            fixture.index_codex();
            assert_eq!(cached_title("Copperwren"), "Updated copied request");
        }

        fs::write(catalog, "").unwrap();
        fixture.index_codex();
        assert_eq!(cached_title("Copperwren"), "Updated copied request");
    }
}

#[test]
fn unreadable_preferred_rollout_hides_archived_text_and_title_until_recovery() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    let fixture = Fixture::new();
    let name = format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl");
    let active = fixture.root.join("codex/sessions/2026/01/02").join(&name);
    let archive = fixture.root.join("codex/archived_sessions");
    fs::create_dir(&archive).unwrap();
    fs::rename(&active, archive.join(name)).unwrap();
    fixture.index_codex();
    let replacement = fixture.root.join("replacement-project");
    fs::create_dir(&replacement).unwrap();
    let compressed = active.with_extension("jsonl.gz");
    let mut writer = GzEncoder::new(Vec::new(), Compression::fast());
    writeln!(
        writer,
        "{}",
        json!({"type":"session_meta","payload":{"id":CODEX_ID, "cwd":replacement}})
    )
    .unwrap();
    writer
        .write_all(b"{\"type\":\"unknown\"}\n".repeat(10_000).as_slice())
        .unwrap();
    let mut damaged = writer.finish().unwrap();
    damaged.truncate(damaged.len() - 8);
    fs::write(&compressed, damaged).unwrap();

    let project = replacement.to_str().unwrap();
    let search = |query, flags: &[&str]| {
        fixture.search_json(
            &[
                &[
                    query,
                    "--project",
                    project,
                    "--provider",
                    "codex",
                    "--show-text",
                ],
                flags,
            ]
            .concat(),
        )
    };
    assert!(result_sessions(&search("Pagination", &["--no-index"])).is_empty());
    let failed = search("Pagination", &[]);
    assert_eq!(failed["index"]["failed"], 1);
    assert!(result_sessions(&failed).is_empty());
    assert_eq!(fixture.index_codex()["failed_skipped"], 1);
    for query in ["Pagination", "backoff"] {
        assert!(result_sessions(&search(query, &[])).is_empty());
        assert!(result_sessions(&search(query, &["--cached"])).is_empty());
    }

    // An interrupted pass can persist the new catalog row before it records a read failure.
    let store = omnis_store::Store::open(fixture.root.join("state/store.sqlite3")).unwrap();
    store
        .clear_trajectory_index_failure(&omnis_ir::SessionRef::new(
            omnis_ir::Provider::Codex,
            CODEX_ID,
        ))
        .unwrap();
    assert!(result_sessions(&search("Pagination", &["--cached"])).is_empty());
    assert!(result_sessions(&search("backoff", &["--cached"])).is_empty());

    fs::remove_file(compressed).unwrap();
    let recovered = fixture.search_json(&["Pagination", "--provider", "codex", "--no-index"]);
    assert_eq!(result_sessions(&recovered), [format!("codex:{CODEX_ID}")]);
    assert_eq!(recovered["index"]["indexed"], 0);
    assert!(result_sessions(&search("Pagination", &["--cached"])).is_empty());
    assert_eq!(
        result_sessions(&fixture.search_json(&["Pagination", "--provider", "codex", "--cached"])),
        [format!("codex:{CODEX_ID}")]
    );
}

#[test]
fn failed_discovery_cannot_assign_a_name_with_two_possible_rollouts() {
    let fixture = Fixture::new();
    let home = fixture.root.join("codex");
    let filename = format!("rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl");
    let selected = home.join("sessions/2026/01/02").join(&filename);
    let archive = home.join("archived_sessions");
    fs::create_dir_all(&archive).unwrap();
    let content = fs::read_to_string(selected).unwrap();
    fs::write(
        archive.join(filename),
        content.replace("Pagination", "Archive"),
    )
    .unwrap();
    fs::write(
        home.join("session_index.jsonl"),
        json!({"id": CODEX_ID, "thread_name": "Violet Quokka"}).to_string(),
    )
    .unwrap();
    let query = ["Violet Quokka", "--provider", "codex", "--cached"];
    fixture.index_codex();
    assert!(result_sessions(&fixture.search_json(&query)).is_empty());
    fs::write(home.join("state_5.sqlite"), "unreadable database").unwrap();
    fixture.index_codex();
    assert!(result_sessions(&fixture.search_json(&query)).is_empty());
}

#[test]
fn live_search_reconciles_a_name_saved_before_the_first_trajectory() {
    use omnis_adapters::{CodexAdapter, ProviderAdapter};
    use omnis_store::{IndexedSession, Store};

    let fixture = Fixture::new();
    let codex = fixture.root.join("codex");
    let native = CodexAdapter::with_root(&codex)
        .find_native_session(CODEX_ID)
        .unwrap()
        .unwrap();
    let state = fixture.root.join("state");
    fs::create_dir_all(&state).unwrap();
    let store = Store::open(state.join("store.sqlite3")).unwrap();
    store
        .upsert_indexed_session(&IndexedSession {
            session: native.session.clone(),
            source_revision: native.source_revision,
            title: Some("Amber Heron".into()),
            project_path: native.project_path,
            git_branch: native.git_branch,
            created_at: native.created_at,
            updated_at: native.updated_at,
            updated_at_approximate: native.updated_at_approximate,
            event_count: native.event_count,
        })
        .unwrap();
    let db = rusqlite::Connection::open(state.join("store.sqlite3")).unwrap();
    assert_eq!(db.execute(
        "UPDATE session_index SET codex_home=?1 WHERE provider='codex' AND session_id=?2 AND title='Amber Heron'
           AND NOT EXISTS (SELECT 1 FROM session_trajectories WHERE provider='codex' AND session_id=?2)",
        rusqlite::params![codex.canonicalize().unwrap().to_str().unwrap(), CODEX_ID],
    ).unwrap(), 1);
    write_jsonl(
        &codex.join("session_index.jsonl"),
        &[json!({
            "id": CODEX_ID, "thread_name": "Violet Quokka"
        })],
    );

    for indexed in [1, 0] {
        let found = fixture.search_json(&["--provider", "codex", "--show-text", "unchanged"]);
        assert_eq!(found["index"]["failed"], 0);
        assert_eq!(found["index"]["indexed"], indexed);
        assert_eq!(result_sessions(&found), [format!("codex:{CODEX_ID}")]);
        assert_eq!(found["results"][0]["title"], "Violet Quokka");
    }
    let cached = fixture.search_json(&["--cached", "--provider", "codex", "--show-text", "Quokka"]);
    assert_eq!(result_sessions(&cached), [format!("codex:{CODEX_ID}")]);
    assert_eq!(cached["results"][0]["title"], "Violet Quokka");
    let recorded_check: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_index_checks)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!recorded_check);
}

#[test]
fn an_unscoped_catalog_name_needs_its_prior_assignment_and_original_rollout() {
    let fixture = Fixture::new();
    let codex = fixture.root.join("codex");
    let names = codex.join("session_index.jsonl");
    let assignment = |id, name| json!({"id": id, "thread_name": name});
    let prior = assignment(CODEX_ID, "Azure Quokka");
    let other = assignment(OTHER_CODEX_ID, "Elm Cormorant");
    fs::write(&names, format!("{prior}\n{other}\n")).unwrap();
    fixture.index_codex();

    // An interrupted older index saved the catalog before writing trajectories or recording the home.
    let state = fixture.root.join("state");
    let populated = fixture.root.join("populated");
    fs::rename(&state, &populated).unwrap();
    fixture.search(&["--cached", "Quokka"]);
    let db = rusqlite::Connection::open(state.join("store.sqlite3")).unwrap();
    db.execute(
        "ATTACH DATABASE ?1 AS populated",
        [populated.join("store.sqlite3").to_str().unwrap()],
    )
    .unwrap();
    db.execute_batch(
        "INSERT INTO session_index
            (provider, session_id, title, project_path, git_branch, created_at, updated_at,
             updated_at_approximate, event_count, indexed_at, source_file_id, source_contents_id)
         SELECT provider, session_id, title, project_path, git_branch, created_at, updated_at,
                updated_at_approximate, event_count, indexed_at, source_file_id, source_contents_id
         FROM populated.session_index WHERE provider='codex'",
    )
    .unwrap();
    drop(db);

    fs::write(&names, format!("{prior}\n{other}\n{{")).unwrap();
    let file = codex.join(format!(
        "sessions/2026/01/02/rollout-2026-01-02T00-00-00-{OTHER_CODEX_ID}.jsonl"
    ));
    let replaced = file.with_extension("previous");
    fs::rename(&file, &replaced).unwrap();
    fs::copy(&replaced, &file).unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(fs::metadata(replaced).unwrap().modified().unwrap())
        .unwrap();

    let cached = |term| fixture.search_json(&["--all-projects", "--cached", "--show-text", term]);
    assert!(result_sessions(&cached("Quokka")).is_empty());
    let shown = |id| {
        let output = fixture
            .command()
            .args(["--json", "show", &format!("codex:{id}")])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(shown(CODEX_ID)["title"], "Azure Quokka");
    assert_eq!(
        result_sessions(&cached("Quokka")),
        [format!("codex:{CODEX_ID}")]
    );
    assert_eq!(cached("unchanged")["results"][0]["title"], "Azure Quokka");
    assert!(shown(OTHER_CODEX_ID)["title"].is_null());
    assert!(result_sessions(&cached("Cormorant")).is_empty());
    assert_eq!(
        cached("zebracorn")["results"][0]["title"],
        "Draft release notes"
    );
    fs::write(names, "").unwrap();
    assert_eq!(fixture.index_codex()["failed"], 0);
    assert!(result_sessions(&cached("Quokka")).is_empty());
    assert_eq!(
        cached("unchanged")["results"][0]["title"],
        "Tune the rate limiter backoff"
    );
}

#[test]
fn codex_search_follows_a_restored_active_rollout_at_an_older_or_equal_time() {
    for (same_project, legacy_index) in [(false, false), (true, false), (true, true)] {
        let fixture = Fixture::new();
        let active = fixture.root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        let archives = fixture.root.join("codex/archived_sessions");
        fs::create_dir(&archives).unwrap();
        let archive = archives.join(active.file_name().unwrap());
        fs::rename(&active, &archive).unwrap();
        let archived_time = archive.metadata().unwrap().modified().unwrap();
        let search = |query, project: &Path, extra: &[&str]| {
            let mut args = vec![
                query,
                "--provider",
                "codex",
                "--project",
                project.to_str().unwrap(),
                "--show-text",
            ];
            args.extend_from_slice(extra);
            fixture.search_json(&args)
        };
        let original = &fixture.workspace;
        let restored = if same_project {
            original.clone()
        } else {
            fixture.root.join("other-workspace")
        };
        assert_eq!(
            search("unchanged", original, &[])["results"][0]["match"],
            "conversation"
        );
        if legacy_index {
            let db = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
            for table in ["session_index", "session_trajectories"] {
                db.execute(&format!("UPDATE {table} SET source_file_id = NULL, source_contents_id = NULL WHERE session_id = ?1"), [CODEX_ID]).unwrap();
            }
        }
        write_codex_session(
            &fixture.root,
            CODEX_ID,
            &restored,
            "Restored request",
            "metronome",
        );
        fs::File::options()
            .write(true)
            .open(&active)
            .unwrap()
            .set_modified(if same_project {
                archived_time
            } else {
                archived_time - std::time::Duration::from_secs(60)
            })
            .unwrap();
        assert!(result_sessions(&search("unchanged", &restored, &["--no-index"])).is_empty());
        let current = search("metronome", &restored, &[]);
        assert_eq!(
            result_sessions(&current),
            [format!("codex:{CODEX_ID}")],
            "{current}"
        );
        assert_eq!(current["results"][0]["title"], "Restored request");
        assert_eq!(current["results"][0]["match"], "conversation");
        assert!(result_sessions(&search("unchanged", original, &[])).is_empty());
        let warm = search("metronome", &restored, &[]);
        assert_eq!(warm["index"]["stale"], 0);
        assert_eq!(result_sessions(&warm), result_sessions(&current));
    }
}

#[test]
fn codex_gui_name_changes_without_reindexing_the_conversation() {
    for (archived, legacy) in [(false, false), (false, true), (true, true)] {
        check_codex_gui_name_changes(archived, legacy);
    }
}

fn check_codex_gui_name_changes(archived: bool, legacy: bool) {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(fixture.root.join("codex/state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, history_mode TEXT, name TEXT)").unwrap();
    let rollout = fixture
        .root
        .join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ))
        .canonicalize()
        .unwrap();
    let source = fs::read_to_string(&rollout).unwrap();
    let (header, conversation) = source.split_once('\n').unwrap();
    let mut header: Value = serde_json::from_str(header).unwrap();
    header["payload"]["history_mode"] = json!("paginated");
    fs::write(&rollout, format!("{header}\n{conversation}")).unwrap();
    if archived {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;

        let directory = fixture.root.join("codex/archived_sessions");
        fs::create_dir(&directory).unwrap();
        let gzip = directory
            .join(rollout.file_name().unwrap())
            .with_extension("jsonl.gz");
        let mut encoder = GzEncoder::new(fs::File::create(gzip).unwrap(), Compression::fast());
        encoder.write_all(&fs::read(&rollout).unwrap()).unwrap();
        encoder.finish().unwrap();
        fs::remove_file(&rollout).unwrap();
    }
    let original = "Xylophonist";
    let renamed = "Vibraphonist";
    let search = |q| fixture.search_json(&[q, "--provider", "codex", "--show-text"]);
    let ordinary = "Carillonneur";
    if !legacy {
        write_jsonl(
            &fixture.root.join("codex/session_index.jsonl"),
            &[json!({"id":CODEX_ID,"thread_name":ordinary})],
        );
        assert_eq!(search(ordinary)["results"][0]["title"], ordinary);
    }
    db.execute(
        "INSERT INTO threads VALUES (?1, ?2, ?3, 'paginated', ?4)",
        rusqlite::params![
            CODEX_ID,
            rollout.to_str(),
            fixture.workspace.to_str(),
            legacy.then_some(original)
        ],
    )
    .unwrap();
    if !legacy {
        let obsolete = fixture.search_json(&[ordinary, "--provider", "codex", "--no-index"]);
        assert!(result_sessions(&obsolete).is_empty(), "{obsolete}");
        db.execute("UPDATE threads SET name = ?1", [original])
            .unwrap();
        let renamed = fixture.search_json(&[ordinary, "--provider", "codex", "--no-index"]);
        assert!(result_sessions(&renamed).is_empty(), "{renamed}");
    }
    assert_eq!(search(original)["results"][0]["title"], original);
    if legacy {
        let cache = rusqlite::Connection::open(fixture.root.join("state/store.sqlite3")).unwrap();
        let rows = cache.execute(
            "UPDATE session_trajectories SET document_version=6, derived_title=?1, derived_title_is_verified=1, derived_title_is_local_opener=0 WHERE provider='codex' AND session_id=?2",
            rusqlite::params![original, CODEX_ID],
        ).unwrap();
        assert_eq!(rows, 1);
    }
    let mut previous = original;
    for name in [Some(renamed), None] {
        db.execute("UPDATE threads SET name = ?1", [name]).unwrap();
        let old_name = fixture.search_json(&[previous, "--provider", "codex", "--no-index"]);
        assert!(
            result_sessions(&old_name).is_empty(),
            "old name {previous}, current {name:?}, archived {archived}, legacy {legacy}: {old_name}"
        );
        if name.is_none() {
            let body = fixture.search_json(&[
                "unchanged",
                "--provider",
                "codex",
                "--no-index",
                "--show-text",
            ]);
            assert_eq!(body["results"][0]["title"], "Tune the rate limiter backoff");
        }
        assert!(result_sessions(&search(original)).is_empty());
        if let Some(name) = name {
            previous = name;
            assert_eq!(search(name)["results"][0]["match"], "metadata");
        } else {
            assert!(result_sessions(&search(renamed)).is_empty());
        }
        let conversation = search("unchanged");
        assert_eq!(conversation["index"]["indexed"], 0);
        assert_eq!(conversation["results"][0]["match"], "conversation");
        assert_eq!(
            conversation["results"][0]["title"],
            name.unwrap_or("Tune the rate limiter backoff")
        );
    }
}

#[test]
fn cached_search_keeps_indexed_history_without_reading_native_stores() {
    let fixture = Fixture::new();
    let indexed = fixture
        .command()
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    fs::remove_dir_all(fixture.root.join("claude")).unwrap();
    fs::remove_dir_all(fixture.root.join("codex")).unwrap();
    let unverified = fixture
        .command()
        .args(["--json", "index", "--provider", "claude"])
        .output()
        .unwrap();
    assert!(unverified.status.success());
    let missing_codex = fixture
        .command()
        .args(["--json", "index", "--provider", "codex"])
        .output()
        .unwrap();
    assert!(missing_codex.status.success());
    // A listing without an explicit completeness guarantee cannot prove deletion.
    let cached = fixture.search_json(&["zebracorn", "--cached", "--show-text"]);
    assert_eq!(result_sessions(&cached), [format!("claude:{CLAUDE_ID}")]);
    assert_eq!(cached["cached"], true);
    assert_eq!(cached["index"]["skipped"], true);
    assert!(cached["index"]["stale"].is_null());
    assert!(!cached.to_string().contains(SECRET));
    assert!(
        cached["warnings"][0]
            .as_str()
            .unwrap()
            .contains("Provider stores were not checked")
    );
    assert!(
        result_sessions(&fixture.search_json(&["zebracorn", "--cached", "--provider", "codex"]))
            .is_empty()
    );
    assert_eq!(
        result_sessions(&fixture.search_json(&[
            "zebracorn",
            "--cached",
            "--all-projects",
            "--provider",
            "codex"
        ])),
        [format!("codex:{OTHER_CODEX_ID}")]
    );
    let mut everywhere =
        result_sessions(&fixture.search_json(&["zebracorn", "--cached", "--all-projects"]));
    everywhere.sort_unstable();
    assert_eq!(
        everywhere,
        [
            format!("claude:{CLAUDE_ID}"),
            format!("codex:{OTHER_CODEX_ID}")
        ]
    );
    let live = fixture.search_json(&["zebracorn", "--no-index"]);
    assert!(result_sessions(&live).is_empty());
}

#[test]
fn cached_search_keeps_other_providers_when_a_home_cannot_be_resolved() {
    let fixture = Fixture::new();
    let indexed = fixture
        .command()
        .args(["index", "--provider", "claude"])
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let inaccessible = fixture.root.join("not-a-directory");
    fs::write(&inaccessible, "").unwrap();
    for (home, should_warn) in [
        (inaccessible.clone(), true),
        (inaccessible.join("provider"), true),
        (fixture.root.join("absent/provider"), false),
    ] {
        let search = fixture
            .command()
            .env("ANTIGRAVITY_CLI_HOME", home)
            .args([
                "--json",
                "search",
                "zebracorn",
                "--cached",
                "--all-projects",
            ])
            .output()
            .unwrap();
        assert!(search.status.success(), "{search:?}");
        let cached: Value = serde_json::from_slice(&search.stdout).unwrap();
        assert_eq!(result_sessions(&cached), [format!("claude:{CLAUDE_ID}")]);
        assert_eq!(
            cached["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("antigravity")),
            should_warn
        );
    }
}

#[test]
fn search_follows_rollout_replacements_rewrites_and_appends() {
    use std::io::Write;

    let fixture = Fixture::new();
    let active = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    let original = fs::read_to_string(&active).unwrap();
    let timestamp = active.metadata().unwrap().modified().unwrap();
    let matches = |query: &str| {
        result_sessions(&fixture.search_json(&[query, "--cached", "--provider", "codex"]))
    };
    assert_eq!(fixture.index_codex()["failed"], 0);
    assert_eq!(matches("limiter").len(), 1);

    let file_id = || omnis_adapters::CodexAdapter::index_file_id(&active).unwrap();
    let before = file_id();
    let replacement = active.with_extension("restore");
    fs::write(&replacement, original.replace("limiter", "warbler")).unwrap();
    fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(timestamp)
        .unwrap();
    fs::rename(replacement, &active).unwrap();
    let restored = file_id();
    assert_ne!(before, restored);
    assert_eq!(fixture.index_codex()["indexed"], 1);
    assert_eq!(matches("warbler").len(), 1);
    assert!(matches("limiter").is_empty());

    // A backup can also overwrite the same file and restore its exact timestamp.
    fs::write(&active, original.replace("limiter", "kinglet")).unwrap();
    fs::File::options()
        .write(true)
        .open(&active)
        .unwrap()
        .set_modified(timestamp)
        .unwrap();
    assert_eq!(restored, file_id());
    #[cfg(unix)]
    assert_eq!(fixture.index_codex()["indexed"], 1);
    let show = fixture
        .command()
        .args(["show", &format!("codex:{CODEX_ID}")])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert_eq!(matches("kinglet").len(), 1);
    assert!(matches("warbler").is_empty());

    // Appending a turn keeps the file identity. Its new modification time refreshes search.
    let mut file = fs::OpenOptions::new().append(true).open(&active).unwrap();
    let turn =
        json!({"type":"event_msg","payload":{"type":"user_message","message":"Followupredtail"}});
    writeln!(file, "{turn}").unwrap();
    file.set_modified(timestamp + std::time::Duration::from_secs(2))
        .unwrap();
    drop(file);
    assert_eq!(restored, file_id());
    assert_eq!(fixture.index_codex()["indexed"], 1);
    assert_eq!(matches("Followupredtail").len(), 1);
    assert_eq!(fixture.index_codex()["stale"], 0);
}

#[test]
fn incomplete_discovery_preserves_cached_metadata_and_history() {
    let fixture = Fixture::new();
    fixture.index_codex();
    let omitted = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{OTHER_CODEX_ID}.jsonl"
    ));
    fs::write(&omitted, "{synthetic malformed metadata}\n").unwrap();
    write_codex_session(
        &fixture.root,
        CODEX_ID,
        &fixture.workspace,
        "Updated synthetic title",
        "The current session is still readable",
    );
    let partial = fixture.index_codex();
    let cached = fixture.search_json(&["zebracorn", "--cached", "--all-projects"]);
    assert_eq!(
        result_sessions(&cached),
        [format!("codex:{OTHER_CODEX_ID}")]
    );
    assert!(
        !partial["notes"].as_array().unwrap().is_empty(),
        "{partial}"
    );
    let updated = fixture.search_json(&["Updated synthetic title", "--cached", "--show-text"]);
    assert_eq!(result_sessions(&updated), [format!("codex:{CODEX_ID}")]);
    assert_eq!(updated["results"][0]["title"], "Updated synthetic title");

    // A later complete listing can remove sessions that are actually gone.
    fs::remove_file(omitted).unwrap();
    let complete = fixture.index_codex();
    assert!(
        complete["notes"].as_array().unwrap().is_empty(),
        "{complete}"
    );
    assert!(
        result_sessions(&fixture.search_json(&["zebracorn", "--cached", "--all-projects"]))
            .is_empty()
    );
}

#[test]
fn a_fully_read_fork_with_missing_history_stays_partial_without_reindexing() {
    for direct_read in [false, true] {
        let fixture = Fixture::new();
        fixture.add_missing_ancestor();
        if direct_read {
            let show = fixture
                .command()
                .args(["show", &format!("codex:{CODEX_ID}")])
                .output()
                .unwrap();
            assert!(
                show.status.success(),
                "{}",
                String::from_utf8_lossy(&show.stderr)
            );
        }
        let first = fixture.index_codex();
        assert_eq!(first["indexed"], if direct_read { 1 } else { 2 });
        assert_eq!(fixture.index_codex()["indexed"], 0);
        let project = fixture.search_json(&["unchanged", "--provider", "codex", "--cached"]);
        assert!(result_sessions(&project).is_empty());
        let cached = fixture.search_json(&[
            "unchanged",
            "--provider",
            "codex",
            "--cached",
            "--all-projects",
        ]);
        assert_eq!(cached["results"][0].get("project_path"), Some(&Value::Null));
        assert_eq!(cached["results"][0]["coverage"], "preview");
    }
}

#[test]
fn empty_cached_search_reports_unknown_freshness() {
    let fixture = Fixture::new();
    let cached = fixture.search_json(&["pagination", "--cached"]);
    assert!(result_sessions(&cached).is_empty());
    assert_eq!(cached["cached"], true);
    assert!(cached["index"]["stale"].is_null());
    assert!(
        cached["warnings"][0]
            .as_str()
            .unwrap()
            .contains("omni index")
    );
}

#[test]
fn conversation_match_follows_delta_indexing_and_second_run_indexes_nothing() {
    let fixture = Fixture::new();

    let first = fixture.search_json(&["zebracorn", "--show-text"]);
    assert_eq!(first["query"], "zebracorn");
    assert_eq!(
        first["index"],
        json!({"candidates": 2, "stale": 2, "indexed": 2, "failed": 0, "failed_skipped": 0, "skipped": false, "interrupted": false})
    );
    assert_eq!(first["has_more"], false);
    assert_eq!(result_sessions(&first), [format!("claude:{CLAUDE_ID}")]);
    let result = &first["results"][0];
    let mut keys = result
        .as_object()
        .expect("result object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "coverage",
            "match",
            "project_path",
            "provider",
            "session",
            "snippet",
            "title",
            "updated_at"
        ]
    );
    assert_eq!(result["provider"], "claude");
    assert_eq!(result["title"], CLAUDE_TITLE);
    assert_eq!(result["match"], "conversation");
    assert_eq!(result["coverage"], "complete");
    assert!(result["updated_at"].is_string());
    assert!(
        result["project_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("workspace"))
    );
    let snippet = result["snippet"].as_str().expect("snippet");
    assert!(snippet.contains("zebracorn"), "{snippet}");
    assert!(snippet.contains("[REDACTED: API_KEY]"), "{snippet}");
    assert!(!first.to_string().contains(SECRET));

    let second = fixture.search_json(&["zebracorn", "--show-text"]);
    assert_eq!(second["index"]["stale"], 0);
    assert_eq!(second["index"]["indexed"], 0);
    assert_eq!(second["results"], first["results"]);

    let text = fixture.search(&["zebracorn", "--show-text"]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(lines[1].starts_with("    [complete] "), "{stdout}");
    assert!(lines[1].contains("zebracorn"), "{stdout}");
    assert!(!stdout.contains(SECRET));

    // Without --show-text, output names the match kind but no title or transcript text.
    let default = fixture.search_json(&["zebracorn"]);
    assert_eq!(default["results"][0]["match"], "conversation");
    assert!(default["results"][0]["title"].is_null());
    assert!(default["results"][0]["snippet"].is_null());
    let plain = fixture.search(&["zebracorn"]);
    let plain = String::from_utf8_lossy(&plain.stdout);
    assert_eq!(plain.lines().count(), 1, "{plain}");
    assert!(plain.trim_end().ends_with("  conversation"), "{plain}");
    assert!(!plain.contains(CLAUDE_TITLE), "{plain}");
    assert!(!plain.contains("Checked"), "{plain}");

    let everywhere = fixture.search_json(&["zebracorn", "--all-projects"]);
    assert_eq!(everywhere["index"]["candidates"], 3);
    assert_eq!(everywhere["index"]["indexed"], 1);
    let mut sessions = result_sessions(&everywhere);
    sessions.sort_unstable();
    assert_eq!(
        sessions,
        [
            format!("claude:{CLAUDE_ID}"),
            format!("codex:{OTHER_CODEX_ID}")
        ]
    );
}

#[test]
fn metadata_matches_rank_first_and_provider_and_limit_filters_apply() {
    let fixture = Fixture::new();

    let text = fixture.search(&["pagination", "--show-text"]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(
        lines[0].starts_with(&format!("claude:{CLAUDE_ID} ")),
        "{stdout}"
    );
    assert!(lines[0].contains("Fix pagination"), "{stdout}");
    assert!(
        lines[1].starts_with(&format!("codex:{CODEX_ID} ")),
        "{stdout}"
    );
    assert!(lines[2].starts_with("    [complete] "), "{stdout}");
    assert!(lines[2].contains("Pagination stays unchanged"), "{stdout}");
    assert!(
        lines.iter().all(|line| line.chars().count() <= 80),
        "{stdout}"
    );

    // Titles derived from indexed conversations count as metadata.
    let derived = fixture.search_json(&["limiter", "--show-text"]);
    assert_eq!(result_sessions(&derived), [format!("codex:{CODEX_ID}")]);
    assert_eq!(derived["results"][0]["match"], "metadata");
    assert_eq!(
        derived["results"][0]["title"],
        "Tune the rate limiter backoff"
    );
    assert!(derived["results"][0]["snippet"].is_null());
    assert!(derived["results"][0]["coverage"].is_null());
    assert!(fixture.search_json(&["limiter"])["results"][0]["title"].is_null());

    let limited = fixture.search_json(&["pagination", "--limit", "1"]);
    assert_eq!(result_sessions(&limited), [format!("claude:{CLAUDE_ID}")]);
    assert_eq!(limited["has_more"], true);
    let limited_text = fixture.search(&["pagination", "--limit", "1"]);
    assert!(
        String::from_utf8_lossy(&limited_text.stdout).ends_with("… more matches; raise --limit\n")
    );

    let codex = fixture.search_json(&["pagination", "--provider", "codex"]);
    assert_eq!(codex["index"]["candidates"], 1);
    assert_eq!(result_sessions(&codex), [format!("codex:{CODEX_ID}")]);
    assert_eq!(codex["results"][0]["match"], "conversation");
}

#[test]
fn quoted_phrase_matches_exact_text_in_titles_and_conversations() {
    let fixture = Fixture::new();
    let title = "019f0000-0000-7000-8000-000000000003";
    let conversation = "019f0000-0000-7000-8000-000000000004";
    let spaced = "019f0000-0000-7000-8000-000000000005";
    let hyphen = "019f0000-0000-7000-8000-000000000006";
    let scattered = "019f0000-0000-7000-8000-000000000007";
    for (id, question, answer) in [
        (
            title,
            "Benchmark Qwen3.8-Coder latency",
            "Numbers look stable",
        ),
        (
            conversation,
            "Compare model runs",
            "The qwen3.8 build finished",
        ),
        (spaced, "Try qwen3 8 quantized", "Slower than expected"),
        (hyphen, "Evaluate the variants", "qwen3-8 is slower"),
        (scattered, "Read qwen3.5 notes", "Retried 8 times"),
    ] {
        write_codex_session(&fixture.root, id, &fixture.workspace, question, answer);
    }

    let quoted = fixture.search_json(&["\"qwen3.8\"", "--show-text"]);
    assert_eq!(
        result_sessions(&quoted),
        [format!("codex:{title}"), format!("codex:{conversation}")]
    );
    assert_eq!(quoted["results"][0]["match"], "metadata");
    assert_eq!(quoted["results"][1]["match"], "conversation");
    let snippet = quoted["results"][1]["snippet"].as_str().expect("snippet");
    assert!(snippet.contains("qwen3.8 build"), "{snippet}");
    let text = fixture.search(&["\"qwen3.8\""]);
    assert_eq!(String::from_utf8_lossy(&text.stdout).lines().count(), 2);

    // Unquoted, punctuation joins tokens without requiring the exact separator.
    let mut unquoted = result_sessions(&fixture.search_json(&["qwen3.8"]));
    unquoted.sort_unstable();
    let mut expected = [title, conversation, spaced, hyphen].map(|id| format!("codex:{id}"));
    expected.sort_unstable();
    assert_eq!(unquoted, expected);

    let empty = fixture
        .command()
        .args(["search", "\"\""])
        .output()
        .expect("run empty quoted search");
    assert!(!empty.status.success());
}

#[test]
fn no_index_searches_only_what_is_already_indexed() {
    let fixture = Fixture::new();

    let empty = fixture.search(&["zebracorn", "--no-index"]);
    assert_eq!(
        String::from_utf8_lossy(&empty.stdout),
        "No sessions match “zebracorn”.\n"
    );

    let metadata = fixture.search_json(&["pagination", "--no-index"]);
    assert_eq!(
        metadata["index"],
        json!({"candidates": 2, "stale": 2, "indexed": 0, "failed": 0, "failed_skipped": 0, "skipped": true, "interrupted": false})
    );
    assert_eq!(result_sessions(&metadata), [format!("claude:{CLAUDE_ID}")]);
    assert_eq!(metadata["results"][0]["match"], "metadata");

    let rejected = fixture
        .command()
        .args(["search", "   "])
        .output()
        .expect("run blank search");
    assert!(!rejected.status.success());
}

#[test]
fn an_unstamped_legacy_document_keeps_only_verified_middle_messages() {
    use std::io::{BufWriter, Write};

    let fixture = Fixture::new();
    let path = fixture.root.join(format!(
        "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
    ));
    let header = json!({"type":"session_meta","payload":{"id":CODEX_ID,"cwd":fixture.workspace}});
    let padding = json!({"type":"event_msg","payload":{"type":"token_count","info":null,"padding":"x".repeat(8192)}}).to_string();
    let write = |middle| {
        let file = fs::File::create(&path).unwrap();
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{header}").unwrap();
        for marker in [middle, "Visible tail"] {
            for _ in 0..1152 {
                writeln!(writer, "{padding}").unwrap();
            }
            let row = json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":marker}]}});
            writeln!(writer, "{row}").unwrap();
        }
        writer.flush().unwrap();
    };
    write("Bronzekestrel");
    let modified = path.metadata().unwrap().modified().unwrap();
    let shown = fixture
        .command()
        .args(["show", &format!("codex:{CODEX_ID}")])
        .output()
        .unwrap();
    assert!(shown.status.success());
    fixture.index_codex();
    let store_path = fixture.root.join("state/store.sqlite3");
    let db = rusqlite::Connection::open(store_path).unwrap();
    let hit = |word| {
        !result_sessions(&fixture.search_json(&[word, "--cached", "--provider", "codex"]))
            .is_empty()
    };
    for substituted in [false, true] {
        db.execute(
            "UPDATE session_trajectories SET document_version=4, source_file_id=NULL, source_contents_id=NULL WHERE session_id=?1",
            [CODEX_ID],
        ).unwrap();
        if substituted {
            write("Topazjackdaw");
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }
        assert_eq!(fixture.index_codex()["indexed"], 1);
        assert_eq!(hit("Bronzekestrel"), !substituted);
        assert!(!hit("Topazjackdaw"));
        assert!(hit("Visible"));
        assert_eq!(fixture.index_codex()["stale"], 0);
        if !substituted {
            // Older readers marked a verified projection partial when the rollout had malformed records.
            for version in 6..=omnis_core::search_document_version(omnis_ir::Provider::Codex) {
                assert_eq!(db.execute(
                    "UPDATE session_trajectories SET document_version=?2, source_complete=0, tail_reader_version=0 WHERE session_id=?1",
                    rusqlite::params![CODEX_ID, version],
                ).unwrap(), 1);
                assert_eq!(fixture.index_codex()["indexed"], 1);
                assert!(hit("Bronzekestrel"));
                assert!(hit("Visible"));
                assert_eq!(fixture.index_codex()["stale"], 0);
            }
        }
    }
}

fn result_sessions(value: &Value) -> Vec<String> {
    value["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|result| result["session"].as_str().expect("session").to_owned())
        .collect()
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary fixture");
        let root = temporary.path().to_path_buf();
        let workspace = root.join("workspace");
        let other_workspace = root.join("other-workspace");
        for directory in [
            workspace.clone(),
            other_workspace.clone(),
            root.join("home"),
            root.join("claude/projects/synthetic"),
            root.join("codex/sessions/2026/01/02"),
        ] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        write_jsonl(
            &root.join(format!("claude/projects/synthetic/{CLAUDE_ID}.jsonl")),
            &[
                json!({
                    "type": "user",
                    "uuid": "44444444-4444-4444-8444-444444444444",
                    "sessionId": CLAUDE_ID,
                    "timestamp": "2026-01-01T00:00:00Z",
                    "cwd": workspace,
                    "message": {"role": "user", "content": CLAUDE_TITLE}
                }),
                json!({
                    "type": "assistant",
                    "uuid": "55555555-5555-4555-8555-555555555555",
                    "parentUuid": "44444444-4444-4444-8444-444444444444",
                    "sessionId": CLAUDE_ID,
                    "timestamp": "2026-01-01T00:00:01Z",
                    "cwd": workspace,
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "text",
                            "text": format!("Checked the zebracorn fixture with api_key={SECRET}")
                        }]
                    }
                }),
            ],
        );
        write_jsonl(
            &root.join("claude/history.jsonl"),
            &[json!({
                "display": CLAUDE_TITLE,
                "project": workspace,
                "sessionId": CLAUDE_ID,
                "timestamp": 1_767_225_600_000_i64
            })],
        );
        write_codex_session(
            &root,
            CODEX_ID,
            &workspace,
            "Tune the rate limiter backoff",
            "Pagination stays unchanged by this answer",
        );
        write_codex_session(
            &root,
            OTHER_CODEX_ID,
            &other_workspace,
            "Draft release notes",
            "The zebracorn fixture appears here too",
        );
        Self {
            _temporary: temporary,
            root,
            workspace,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omni"));
        command
            .current_dir(&self.workspace)
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("APPDATA", self.root.join("home/appdata"))
            .env("LOCALAPPDATA", self.root.join("home/localappdata"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg/config"))
            .env("XDG_DATA_HOME", self.root.join("xdg/data"))
            .env("XDG_STATE_HOME", self.root.join("xdg/state"))
            .env("XDG_CACHE_HOME", self.root.join("xdg/cache"))
            .env("OMNISESSION_HOME", self.root.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CODEX_SQLITE_HOME", self.root.join("codex"))
            .env("OMNI_NO_UPDATE_CHECK", "1");
        // OpenCode discovery runs `opencode` from PATH, so keep real installations out of reach.
        #[cfg(unix)]
        command.env("PATH", "/usr/bin:/bin");
        for variable in [
            "GROK_HOME",
            "HERMES_HOME",
            "ANTIGRAVITY_CLI_HOME",
            "ANTIGRAVITY_IDE_HOME",
            "ANTIGRAVITY_DESKTOP_HOME",
            "ANTIGRAVITY_IDE_USER_DATA_DIR",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "CURSOR_AGENT_HOME",
            "CURSOR_CONFIG_DIR",
            "CURSOR_IDE_HOME",
            "OMNI_CLAUDE_BIN",
            "OMNI_CODEX_BIN",
            "OMNI_OPENCODE_BIN",
            "OMNI_GROK_BIN",
            "OMNI_HERMES_BIN",
            "OMNI_ANTIGRAVITY_BIN",
            "OMNI_PI_BIN",
            "OMNI_CURSOR_AGENT_BIN",
            "OMNI_CURSOR_IDE_BIN",
        ] {
            command.env(variable, self.root.join("missing").join(variable));
        }
        command
    }

    fn search(&self, args: &[&str]) -> Output {
        let output = self
            .command()
            .arg("search")
            .args(args)
            .output()
            .expect("run omni search");
        assert!(
            output.status.success(),
            "omni search {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn search_json(&self, args: &[&str]) -> Value {
        let output = self.search(&[&["--json"][..], args].concat());
        serde_json::from_slice(&output.stdout).expect("search JSON")
    }

    fn add_missing_ancestor(&self) {
        let path = self.root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{CODEX_ID}.jsonl"
        ));
        let source = fs::read_to_string(&path).unwrap();
        let (header, conversation) = source.split_once('\n').unwrap();
        let mut header: Value = serde_json::from_str(header).unwrap();
        header["payload"]["history_mode"] = json!("paginated");
        header["payload"]["history_base"] = json!({
            "thread_id": "019f0000-0000-7000-8000-000000000099",
            "end_byte_offset": 512, "end_ordinal_exclusive": 4
        });
        fs::write(path, format!("{header}\n{conversation}")).unwrap();
    }

    fn index_codex(&self) -> Value {
        let output = self
            .command()
            .args(["--json", "index", "--provider", "codex"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn write_codex_session(root: &Path, id: &str, workspace: &Path, question: &str, answer: &str) {
    write_jsonl(
        &root.join(format!(
            "codex/sessions/2026/01/02/rollout-2026-01-02T00-00-00-{id}.jsonl"
        )),
        &[
            json!({
                "type": "session_meta",
                "timestamp": "2026-01-02T00:00:00Z",
                "payload": {"id": id, "cwd": workspace}
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": question}]
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": answer}]
                }
            }),
        ],
    );
}

fn write_jsonl(path: &Path, records: &[Value]) {
    let mut contents = String::new();
    for record in records {
        contents.push_str(&record.to_string());
        contents.push('\n');
    }
    fs::write(path, contents).expect("write synthetic JSONL");
}
