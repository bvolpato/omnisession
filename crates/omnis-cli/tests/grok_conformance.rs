use std::{env, fs, path::PathBuf, process::Command};

use serde_json::json;

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
#[ignore = "requires OMNI_TEST_GROK_BIN"]
fn installed_grok_round_trips_isolated_synthetic_history() {
    let grok_binary = env::var_os("OMNI_TEST_GROK_BIN")
        .map(PathBuf::from)
        .expect("OMNI_TEST_GROK_BIN");
    let temporary = tempfile::tempdir().expect("temporary conformance root");
    let home = temporary.path().join("home");
    let codex_home = temporary.path().join("codex");
    let grok_home = temporary.path().join("grok");
    let omnisession_home = temporary.path().join("omnisession");
    let workspace = temporary.path().join("workspace");
    let sessions = codex_home.join("sessions/2026/01/01");
    for directory in [&home, &grok_home, &workspace, &sessions] {
        fs::create_dir_all(directory).expect("isolated conformance directory");
    }
    let rollout = sessions.join(format!("rollout-2026-01-01T00-00-00-{SOURCE_ID}.jsonl"));
    fs::write(&rollout, synthetic_codex_rollout(&workspace)).expect("synthetic Codex rollout");

    let output = Command::new(env!("CARGO_BIN_EXE_omnis"))
        .args([
            "resume",
            &format!("codex:{SOURCE_ID}"),
            "--in",
            "grok",
            "--materialize-only",
            "--allow-workspace-mismatch",
        ])
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .env("GROK_HOME", &grok_home)
        .env("OMNISESSION_HOME", &omnisession_home)
        .env("OMNI_GROK_BIN", &grok_binary)
        .output()
        .expect("run OmniSession Grok conformance");

    assert!(
        output.status.success(),
        "Grok conformance failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Created and verified grok:"),
        "Grok conformance omitted verification confirmation"
    );
}

fn synthetic_codex_rollout(workspace: &std::path::Path) -> String {
    let mut records = vec![json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": SOURCE_ID,
            "cwd": workspace,
            "cli_version": "synthetic",
            "model_provider": "synthetic"
        }
    })];
    records.push(message_record("user", "Synthetic opening question"));
    for index in 0..90 {
        records.push(json!({
            "timestamp": "2026-01-01T00:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": format!("synthetic-{index}"),
                "output": if index == 42 {
                    "secret=synthetic-value".to_owned()
                } else {
                    format!("Synthetic documentary result {index}")
                }
            }
        }));
    }
    for index in 0_usize..10 {
        let role = if index % 2 == 0 { "assistant" } else { "user" };
        records.push(message_record(
            role,
            &format!("Synthetic visible message {index}"),
        ));
    }
    records
        .into_iter()
        .map(|record| serde_json::to_string(&record).expect("serialize synthetic record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn message_record(role: &str, text: &str) -> serde_json::Value {
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
