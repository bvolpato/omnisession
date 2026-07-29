use std::{env, fs, path::PathBuf, process::Command};

use serde_json::json;

const CODEX_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CLAUDE_SOURCE_ID: &str = "22222222-2222-4222-8222-222222222222";

#[test]
#[ignore = "requires OMNI_TEST_CLAUDE_BIN"]
fn installed_claude_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "claude",
        "OMNI_TEST_CLAUDE_BIN",
        "OMNI_CLAUDE_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_CODEX_BIN"]
fn installed_codex_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_claude_source();
    fixture.assert_materializes(
        &format!("claude:{CLAUDE_SOURCE_ID}"),
        "codex",
        "OMNI_TEST_CODEX_BIN",
        "OMNI_CODEX_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_BIN"]
fn installed_cursor_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "cursor",
        "OMNI_TEST_CURSOR_BIN",
        "OMNI_CURSOR_AGENT_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_PI_BIN"]
fn installed_pi_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "pi",
        "OMNI_TEST_PI_BIN",
        "OMNI_PI_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_ANTIGRAVITY_BIN"]
fn installed_antigravity_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "antigravity",
        "OMNI_TEST_ANTIGRAVITY_BIN",
        "OMNI_ANTIGRAVITY_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires OMNI_TEST_CURSOR_IDE_BIN"]
fn installed_cursor_ide_round_trips_isolated_synthetic_history() {
    let fixture = Fixture::new();
    let source = fixture.write_codex_source();
    fixture.assert_materializes(
        &format!("codex:{CODEX_SOURCE_ID}"),
        "cursor-ide",
        "OMNI_TEST_CURSOR_IDE_BIN",
        "OMNI_CURSOR_IDE_BIN",
        &source,
    );
}

#[test]
#[ignore = "requires all five OMNI_TEST_*_BIN variables"]
fn installed_five_by_five_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((target, fixture.materialize(&seed, target, &binaries)));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries);
            completed += 1;
        }
    }

    assert_eq!(completed, 20);
}

#[test]
#[ignore = "requires all six OMNI_TEST_*_BIN variables"]
fn installed_six_by_six_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
        ("pi", "OMNI_TEST_PI_BIN", "OMNI_PI_BIN"),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((target, fixture.materialize(&seed, target, &binaries)));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries);
            completed += 1;
        }
    }

    assert_eq!(completed, 30);
}

#[test]
#[ignore = "requires all eight OMNI_TEST_*_BIN variables"]
fn installed_eight_by_eight_cross_provider_matrix() {
    let binaries = [
        ("claude", "OMNI_TEST_CLAUDE_BIN", "OMNI_CLAUDE_BIN"),
        ("codex", "OMNI_TEST_CODEX_BIN", "OMNI_CODEX_BIN"),
        ("opencode", "OMNI_TEST_OPENCODE_BIN", "OMNI_OPENCODE_BIN"),
        ("grok", "OMNI_TEST_GROK_BIN", "OMNI_GROK_BIN"),
        (
            "antigravity",
            "OMNI_TEST_ANTIGRAVITY_BIN",
            "OMNI_ANTIGRAVITY_BIN",
        ),
        ("pi", "OMNI_TEST_PI_BIN", "OMNI_PI_BIN"),
        ("cursor", "OMNI_TEST_CURSOR_BIN", "OMNI_CURSOR_AGENT_BIN"),
        (
            "cursor-ide",
            "OMNI_TEST_CURSOR_IDE_BIN",
            "OMNI_CURSOR_IDE_BIN",
        ),
    ]
    .map(|(provider, test_variable, runtime_variable)| {
        let binary =
            env::var_os(test_variable).map_or_else(|| panic!("{test_variable}"), PathBuf::from);
        (provider, runtime_variable, binary)
    });
    let fixture = Fixture::new();
    fixture.write_codex_source();
    let seed = format!("codex:{CODEX_SOURCE_ID}");
    let mut sources = vec![("codex", seed.clone())];
    let mut completed = 0;

    for (target, _, _) in &binaries {
        if *target == "codex" {
            continue;
        }
        sources.push((target, fixture.materialize(&seed, target, &binaries)));
        completed += 1;
    }

    for (source_provider, source) in &sources {
        for (target, _, _) in &binaries {
            if source_provider == target || *source_provider == "codex" {
                continue;
            }
            fixture.materialize(source, target, &binaries);
            completed += 1;
        }
    }

    assert_eq!(completed, 56);
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
    cursor_chats: PathBuf,
    pi_sessions: PathBuf,
    antigravity: PathBuf,
    cursor_ide: PathBuf,
    opencode_database: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary conformance root");
        let root = temporary
            .path()
            .canonicalize()
            .expect("canonical conformance root");
        let fixture = Self {
            home: root.join("home"),
            workspace: root.join("workspace"),
            claude: root.join("claude"),
            codex: root.join("codex"),
            cursor_chats: root.join("cursor/chats"),
            pi_sessions: root.join("pi/sessions"),
            antigravity: root.join("antigravity"),
            cursor_ide: root.join("cursor-ide/User"),
            opencode_database: root.join("opencode.db"),
            root,
            _temporary: temporary,
        };
        for directory in [
            &fixture.home,
            &fixture.workspace,
            &fixture.claude,
            &fixture.codex,
            &fixture.cursor_chats,
            &fixture.pi_sessions,
            &fixture.antigravity,
            &fixture.cursor_ide,
        ] {
            fs::create_dir_all(directory).expect("isolated conformance directory");
        }
        fixture.create_antigravity_store();
        fixture.create_cursor_ide_store();
        fixture
    }

    fn write_codex_source(&self) -> PathBuf {
        let directory = self.codex.join("sessions/2026/01/01");
        fs::create_dir_all(&directory).expect("Codex source directory");
        let source = directory.join(format!(
            "rollout-2026-01-01T00-00-00-{CODEX_SOURCE_ID}.jsonl"
        ));
        let records = [
            json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": CODEX_SOURCE_ID,
                    "cwd": self.workspace,
                    "cli_version": "synthetic",
                    "model_provider": "synthetic"
                }
            }),
            codex_message("user", "Synthetic opening question"),
            codex_message("assistant", "Synthetic opening answer"),
            json!({
                "timestamp": "2026-01-01T00:00:03Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "synthetic-tool",
                    "output": "secret=synthetic-value"
                }
            }),
            codex_message("user", "Synthetic final question"),
            codex_message("assistant", "Synthetic final answer"),
        ];
        let document = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&source, document).expect("synthetic Codex source");
        source
    }

    fn write_claude_source(&self) -> PathBuf {
        let directory = self.claude.join("projects/synthetic");
        fs::create_dir_all(&directory).expect("Claude source directory");
        let source = directory.join(format!("{CLAUDE_SOURCE_ID}.jsonl"));
        let records = [
            json!({
                "type": "user",
                "uuid": "33333333-3333-4333-8333-333333333333",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": self.workspace,
                "message": {"role": "user", "content": "Synthetic opening question"}
            }),
            json!({
                "type": "assistant",
                "uuid": "44444444-4444-4444-8444-444444444444",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:01Z",
                "cwd": self.workspace,
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Synthetic opening answer"},
                        {
                            "type": "tool_use",
                            "id": "synthetic-tool",
                            "name": "Read",
                            "input": {"path": "synthetic.txt"}
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "55555555-5555-4555-8555-555555555555",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:02Z",
                "cwd": self.workspace,
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "synthetic-tool",
                        "content": "secret=synthetic-value"
                    }]
                }
            }),
            json!({
                "type": "user",
                "uuid": "66666666-6666-4666-8666-666666666666",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:03Z",
                "cwd": self.workspace,
                "message": {"role": "user", "content": "Synthetic final question"}
            }),
            json!({
                "type": "assistant",
                "uuid": "77777777-7777-4777-8777-777777777777",
                "sessionId": CLAUDE_SOURCE_ID,
                "timestamp": "2026-01-01T00:00:04Z",
                "cwd": self.workspace,
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Synthetic final answer"}]
                }
            }),
        ];
        let document = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&source, document).expect("synthetic Claude source");
        source
    }

    fn assert_materializes(
        &self,
        source: &str,
        target: &str,
        test_binary_variable: &str,
        binary_variable: &str,
        source_path: &std::path::Path,
    ) {
        let binary = env::var_os(test_binary_variable)
            .map_or_else(|| panic!("{test_binary_variable}"), PathBuf::from);
        let source_before = fs::read(source_path).expect("source before import");
        let output = self
            .command(source, target)
            .env(binary_variable, &binary)
            .output()
            .expect("run installed native conformance");

        assert_success(target, &output);
        assert_eq!(
            fs::read(source_path).expect("source after import"),
            source_before,
            "{target} import changed source session"
        );
    }

    fn materialize(
        &self,
        source: &str,
        target: &str,
        binaries: &[(&str, &str, PathBuf)],
    ) -> String {
        let mut command = self.command(source, target);
        for (_, variable, binary) in binaries {
            command.env(variable, binary);
        }
        let output = command.output().expect("run installed matrix cell");
        assert_success(target, &output);
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 conformance output");
        stdout
            .lines()
            .find_map(|line| {
                line.strip_prefix("Created and verified ")
                    .and_then(|value| value.strip_suffix('.'))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("{target} conformance omitted target session ID"))
    }

    fn command(&self, source: &str, target: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_omnis"));
        command
            .args(["resume", source, "--in", target, "--materialize-only"])
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .env("CURSOR_AGENT_HOME", &self.cursor_chats)
            .env("PI_CODING_AGENT_SESSION_DIR", &self.pi_sessions)
            .env("ANTIGRAVITY_CLI_HOME", &self.antigravity)
            .env("CURSOR_IDE_HOME", &self.cursor_ide)
            .env("GROK_HOME", self.root.join("grok"))
            .env("OMNISESSION_HOME", self.root.join("omnisession"))
            .env("OPENCODE_TEST_HOME", &self.home)
            .env("OPENCODE_DB", &self.opencode_database)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_CONFIG_CONTENT", "{}");
        command
    }

    fn create_antigravity_store(&self) {
        let connection =
            rusqlite::Connection::open(self.antigravity.join("conversation_summaries.db"))
                .expect("Antigravity summary database");
        connection
            .execute_batch(
                r"
                CREATE TABLE conversation_summaries (
                    conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT '',
                    preview text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0,
                    last_modified_time datetime NOT NULL, workspace_uris text NOT NULL,
                    status text NOT NULL DEFAULT '', source text NOT NULL DEFAULT '',
                    project_id text NOT NULL DEFAULT '', agent_name text NOT NULL DEFAULT '',
                    parent_conversation_id text NOT NULL DEFAULT '', nesting_depth integer NOT NULL DEFAULT 0,
                    battle_id text NOT NULL DEFAULT '', winning_conversation_id text NOT NULL DEFAULT '',
                    not_fully_idle numeric NOT NULL DEFAULT false, killed numeric NOT NULL DEFAULT false,
                    last_user_input_time datetime NOT NULL, last_user_input_step_index integer NOT NULL DEFAULT -1,
                    app_data_dir text NOT NULL DEFAULT ''
                );
                PRAGMA user_version = 1;
                ",
            )
            .expect("Antigravity summary schema");
    }

    fn create_cursor_ide_store(&self) {
        let workspace_id = "cursor-ide-fixture";
        let workspace_root = self.cursor_ide.join("workspaceStorage").join(workspace_id);
        fs::create_dir_all(self.cursor_ide.join("globalStorage"))
            .expect("Cursor IDE global storage");
        fs::create_dir_all(&workspace_root).expect("Cursor IDE workspace storage");
        fs::write(
            workspace_root.join("workspace.json"),
            serde_json::to_vec(&json!({
                "folder": format!("file://{}", self.workspace.display())
            }))
            .expect("Cursor IDE workspace JSON"),
        )
        .expect("Cursor IDE workspace metadata");
        let connection =
            rusqlite::Connection::open(self.cursor_ide.join("globalStorage/state.vscdb"))
                .expect("Cursor IDE state database");
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT);
                 CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
                 CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .expect("Cursor IDE state schema");
    }
}

fn assert_success(target: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{target} conformance failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Created and verified "),
        "{target} conformance omitted verification confirmation"
    );
}

fn codex_message(role: &str, text: &str) -> serde_json::Value {
    json!({
        "timestamp": "2026-01-01T00:00:01Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": role,
            "content": [{
                "type": if role == "user" { "input_text" } else { "output_text" },
                "text": text
            }]
        }
    })
}
