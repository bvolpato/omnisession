use std::{fs, path::Path, process::Command};

use anyhow::Result;
use serde_json::{Value, json};

const STAMP: &str = "2026-01-01T00:00:00Z";
const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const ENV_GENERATION: &str = "22222222-2222-4222-8222-222222222222";
const CONFIG_GENERATION: &str = "33333333-3333-4333-8333-333333333333";

fn write_rollout(path: &Path, thread: &str, workspace: &Path, text: &str) -> Result<()> {
    let header = json!({"type":"session_meta","ordinal":0,"timestamp":STAMP,"payload":{
        "id":thread,"cwd":workspace,"history_mode":"paginated"
    }});
    let raw = json!({"type":"response_item","ordinal":1,"timestamp":STAMP,"payload":{
        "type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":text}]
    }});
    let displayed = json!({"type":"event_msg","ordinal":2,"timestamp":STAMP,"payload":{
        "type":"item_completed","thread_id":thread,"turn_id":"turn-one",
        "item":{"type":"AgentMessage","id":"answer","content":[{"type":"Text","text":text}]}
    }});
    fs::write(path, format!("{header}\n{raw}\n{displayed}\n"))?;
    Ok(())
}

fn select(directory: &Path, path: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let db = rusqlite::Connection::open(directory.join("state_5.sqlite"))?;
    db.execute_batch(
        "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT)",
    )?;
    db.execute(
        "INSERT INTO threads VALUES (?1, ?2, 'paginated')",
        [THREAD, path.to_str().unwrap()],
    )?;
    Ok(())
}

fn shown(root: &Path, environment: Option<&Path>) -> Result<Option<Vec<String>>> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_omni"));
    cmd.current_dir(root.join("workspace"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("xdg-cache"))
        .env("XDG_DATA_HOME", root.join("xdg-data"))
        .env("OMNISESSION_HOME", root.join("omni"))
        .env("OMNI_NO_UPDATE_CHECK", "1")
        .env("CODEX_HOME", root.join("codex"))
        .env("OMNI_CODEX_BIN", root.join("bin/codex"))
        .args(["--json", "show", &format!("codex:{THREAD}")]);
    if let Some(environment) = environment {
        cmd.env("CODEX_SQLITE_HOME", environment);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let snapshot: Value = serde_json::from_slice(&output.stdout)?;
    let answers = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "message_assistant")
        .map(|event| event["payload"]["text"].as_str().unwrap().to_owned())
        .collect();
    Ok(Some(answers))
}

#[test]
fn configured_sqlite_home_controls_which_generation_of_a_thread_is_visible() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let home = root.join("codex");
    let environment = root.join("environment");
    let configured = root.join("configured state");
    let workspace = root.join("workspace");
    let day = home.join("sessions/2026/01/01");
    fs::create_dir_all(&day)?;
    fs::create_dir(&workspace)?;
    fs::create_dir(root.join("home"))?;
    let stem = format!("rollout-2026-01-01T00-00-00-{THREAD}");
    let original = day.join(format!("{stem}.jsonl"));
    let alternate = day.join(format!("{stem}_{ENV_GENERATION}.jsonl"));
    let current = day.join(format!("{stem}_{CONFIG_GENERATION}.jsonl"));
    for (path, directory, answer) in [
        (&original, &home, "default answer"),
        (&alternate, &environment, "environment answer"),
        (&current, &configured, "configured answer"),
    ] {
        write_rollout(path, THREAD, &workspace, answer)?;
        select(directory, path)?;
    }
    assert_eq!(shown(&root, None)?, Some(vec!["default answer".into()]));
    assert_eq!(
        shown(&root, Some(&environment))?,
        Some(vec!["environment answer".into()])
    );
    let config = home.join("config.toml");
    fs::write(&config, "sqlite_home = '../configured state'\n")?;
    assert_eq!(
        shown(&root, Some(&environment))?,
        Some(vec!["configured answer".into()])
    );
    let project_config = workspace.join(".codex");
    fs::create_dir(&project_config)?;
    fs::write(
        project_config.join("config.toml"),
        format!("sqlite_home = {}\n", serde_json::to_string(&configured)?),
    )?;
    // Codex writes the canonical trust key with the ordinary drive spelling when possible.
    let mut key = dunce::canonicalize(&workspace)?
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) {
        key.make_ascii_lowercase();
    }
    let trust = format!("[projects.{}]", serde_json::to_string(&key)?);
    fs::write(
        &config,
        format!("sqlite_home = 42\n{trust}\ntrust_level = 'untrusted'\n"),
    )?;
    assert_eq!(shown(&root, Some(&environment))?, None);
    fs::write(
        &config,
        format!("sqlite_home = 42\n{trust}\ntrust_level = 'trusted'\n"),
    )?;
    assert_eq!(
        shown(&root, Some(&environment))?,
        Some(vec!["configured answer".into()])
    );
    fs::remove_file(project_config.join("config.toml"))?;
    fs::write(&config, "sqlite_home = '../configured state'\n")?;

    write_rollout(&current, ENV_GENERATION, &workspace, "wrong thread")?;
    assert_eq!(shown(&root, Some(&environment))?, None);
    fs::remove_file(&current)?;
    assert_eq!(shown(&root, Some(&environment))?, None);
    fs::write(config, "sqlite_home = '../missing state'\n")?;
    assert_eq!(shown(&root, Some(&environment))?, None);
    Ok(())
}

#[cfg(unix)]
#[test]
fn tilde_in_the_sqlite_environment_selects_the_installed_versions_rollout() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let workspace = root.join("workspace");
    let day = root.join("codex/sessions/2026/01/01");
    let binary = root.join("bin/codex");
    fs::create_dir_all(&day)?;
    fs::create_dir_all(binary.parent().unwrap())?;
    fs::create_dir_all(&workspace)?;
    let literal = workspace.join("~/state");
    let expanded = root.join("home/state");
    let environment = Some(Path::new("~/state"));
    let original = day.join(format!("rollout-2026-01-01T00-00-00-{THREAD}.jsonl"));
    write_rollout(&original, THREAD, &workspace, "filesystem selection")?;
    assert_eq!(
        shown(&root, environment)?,
        Some(vec!["filesystem selection".into()])
    );
    for (generation, directory, answer) in [
        (ENV_GENERATION, literal, "literal selection"),
        (CONFIG_GENERATION, expanded, "expanded selection"),
    ] {
        let rollout = day.join(format!(
            "rollout-2026-01-01T00-00-00-{THREAD}_{generation}.jsonl"
        ));
        write_rollout(&rollout, THREAD, &workspace, answer)?;
        select(&directory, &rollout)?;
    }
    let probes = root.join("bin/codex.probes");
    for (version, expected) in [
        ("0.137.0", Some("literal selection")),
        ("0.167.1", Some("expanded selection")),
        ("0.999.0", None),
    ] {
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && [ -z \"$CODEX_SQLITE_HOME\" ] && [ ! -e \"$CODEX_HOME/sessions\" ] || exit 1\necho probe >> \"$0.probes\"\necho 'codex-cli {version}'\n"
            ),
        )?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
        assert_eq!(
            shown(&root, environment)?,
            expected.map(|text| vec![text.to_owned()])
        );
        assert_eq!(fs::read_to_string(&probes)?, "probe\n");
        fs::remove_file(&probes)?;
    }
    fs::remove_file(binary)?;
    assert_eq!(shown(&root, environment)?, None);
    Ok(())
}
