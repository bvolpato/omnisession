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
fn conversation_match_follows_delta_indexing_and_second_run_indexes_nothing() {
    let fixture = Fixture::new();

    let first = fixture.search_json(&["zebracorn"]);
    assert_eq!(first["query"], "zebracorn");
    assert_eq!(
        first["index"],
        json!({"candidates": 2, "stale": 2, "indexed": 2, "failed": 0, "skipped": false, "interrupted": false})
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

    let second = fixture.search_json(&["zebracorn"]);
    assert_eq!(second["index"]["stale"], 0);
    assert_eq!(second["index"]["indexed"], 0);
    assert_eq!(second["results"], first["results"]);

    let text = fixture.search(&["zebracorn"]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(lines[1].starts_with("    [complete] "), "{stdout}");
    assert!(lines[1].contains("zebracorn"), "{stdout}");
    assert!(!stdout.contains(SECRET));

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

    let text = fixture.search(&["pagination"]);
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
    let derived = fixture.search_json(&["limiter"]);
    assert_eq!(result_sessions(&derived), [format!("codex:{CODEX_ID}")]);
    assert_eq!(derived["results"][0]["match"], "metadata");
    assert_eq!(
        derived["results"][0]["title"],
        "Tune the rate limiter backoff"
    );
    assert!(derived["results"][0]["snippet"].is_null());
    assert!(derived["results"][0]["coverage"].is_null());

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
        json!({"candidates": 2, "stale": 2, "indexed": 0, "failed": 0, "skipped": true, "interrupted": false})
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
            .env("OMNI_NO_UPDATE_CHECK", "1");
        // OpenCode discovery runs `opencode` from PATH, so keep real installations out of reach.
        #[cfg(unix)]
        command.env("PATH", "/usr/bin:/bin");
        for variable in [
            "GROK_HOME",
            "HERMES_HOME",
            "ANTIGRAVITY_CLI_HOME",
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
