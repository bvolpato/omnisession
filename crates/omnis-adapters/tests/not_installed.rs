//! Missing provider binaries and stores are normal. Only real failures may warn.

use std::{env, fs, path::Path, process::Command};

use omnis_adapters::{
    AdapterRegistry, AntigravityAdapter, CursorIdeAdapter, HermesAdapter, ProviderAdapter,
};
use omnis_ir::Provider;

const SCENARIO: &str = "OMNISESSION_ADAPTER_TEST_SCENARIO";

/// Reruns one test in a child process whose provider environment points into an empty home.
///
/// The child owns its environment, so no test mutates process-global state.
fn run_in_synthetic_home(test: &str, scenario: &str, prepare_path: impl FnOnce(&Path)) {
    let home = tempfile::tempdir().expect("synthetic home");
    let root = home.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("synthetic PATH directory");
    prepare_path(&bin);

    let mut command = Command::new(env::current_exe().expect("test executable"));
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(SCENARIO, scenario)
        .env("PATH", &bin)
        .env("HOME", root)
        .env("USERPROFILE", root);
    for (name, relative) in [
        ("APPDATA", "AppData/Roaming"),
        ("LOCALAPPDATA", "AppData/Local"),
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_DATA_HOME", ".local/share"),
        ("OMNISESSION_HOME", ".omnisession"),
        ("CLAUDE_CONFIG_DIR", ".claude"),
        ("CODEX_HOME", ".codex"),
        ("GROK_HOME", ".grok"),
        ("HERMES_HOME", ".hermes"),
        ("ANTIGRAVITY_CLI_HOME", ".gemini/antigravity-cli"),
        ("PI_CODING_AGENT_DIR", ".pi/agent"),
        ("PI_CODING_AGENT_SESSION_DIR", ".pi/agent/sessions"),
        ("CURSOR_AGENT_HOME", ".cursor/chats"),
        ("CURSOR_CONFIG_DIR", ".cursor"),
        ("CURSOR_IDE_HOME", "Cursor/User"),
    ] {
        command.env(name, root.join(relative));
    }

    let output = command.output().expect("child test process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(&format!("test {test} ... ok")),
        "child test failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn in_child() -> bool {
    env::var_os(SCENARIO).is_some()
}

#[test]
fn every_provider_lists_an_empty_home_without_warnings() {
    if !in_child() {
        run_in_synthetic_home(
            "every_provider_lists_an_empty_home_without_warnings",
            "empty-home",
            |_| {},
        );
        return;
    }

    let home = env::var_os("HOME").expect("synthetic HOME");
    let registry = AdapterRegistry::with_local_adapters();
    let mut warnings = Vec::new();
    let mut listed = Vec::new();
    for provider in Provider::ALL.iter().copied() {
        let Some(adapter) = registry.get(provider) else {
            continue;
        };
        listed.push(provider);
        if let Some(root) = adapter.probe().data_root {
            assert!(
                root.starts_with(&home),
                "{provider} data root escaped the synthetic home"
            );
        }
        match registry.list_sessions_with_notes(provider, None) {
            Ok((sessions, notes)) => {
                assert!(sessions.is_empty(), "{provider} listed sessions");
                warnings.extend(notes.into_iter().map(|note| format!("{provider}: {note}")));
            }
            Err(error) => warnings.push(format!("{provider}: {error:#}")),
        }
    }

    // These three warned before: a missing `opencode` binary, Cursor IDE database, and Antigravity
    // summary database.
    for provider in [
        Provider::OpenCode,
        Provider::CursorIde,
        Provider::Antigravity,
    ] {
        assert!(listed.contains(&provider), "{provider} was not listed");
    }
    assert_eq!(listed.len(), 9, "{listed:?}");
    assert!(warnings.is_empty(), "{warnings:#?}");
}

#[test]
fn missing_stores_under_existing_provider_roots_do_not_warn() {
    let temporary = tempfile::tempdir().expect("temporary directory");

    let cursor = temporary.path().join("cursor");
    fs::create_dir_all(cursor.join("globalStorage")).expect("Cursor storage without database");
    let antigravity = temporary.path().join("antigravity");
    fs::create_dir_all(&antigravity).expect("Antigravity root without summaries");
    let hermes = temporary.path().join("hermes");
    fs::create_dir_all(&hermes).expect("Hermes root without state database");

    for adapter in [
        &CursorIdeAdapter::with_root(&cursor) as &dyn ProviderAdapter,
        &AntigravityAdapter::with_root(&antigravity),
        &HermesAdapter::with_root(&hermes),
    ] {
        let sessions = adapter
            .list_sessions(None)
            .unwrap_or_else(|error| panic!("{}: {error:#}", adapter.provider()));
        assert!(sessions.is_empty());
        assert!(adapter.discovery_notes().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn opencode_spawn_failures_other_than_a_missing_binary_still_warn() {
    use std::os::unix::fs::PermissionsExt;

    if !in_child() {
        run_in_synthetic_home(
            "opencode_spawn_failures_other_than_a_missing_binary_still_warn",
            "opencode-not-executable",
            |bin| {
                let opencode = bin.join("opencode");
                fs::write(&opencode, "#!/bin/sh\n").expect("synthetic OpenCode");
                fs::set_permissions(&opencode, fs::Permissions::from_mode(0o644))
                    .expect("non-executable OpenCode");
            },
        );
        return;
    }

    let error = AdapterRegistry::with_local_adapters()
        .list_sessions_with_notes(Provider::OpenCode, None)
        .expect_err("non-executable OpenCode must warn");
    assert!(error.to_string().contains("opencode"), "{error:#}");
}

#[test]
fn unreadable_provider_stores_still_warn() {
    let temporary = tempfile::tempdir().expect("temporary directory");

    let cursor = temporary.path().join("cursor");
    fs::create_dir_all(cursor.join("globalStorage")).expect("Cursor storage");
    fs::write(
        cursor.join("globalStorage/state.vscdb"),
        b"not a sqlite database",
    )
    .expect("corrupt Cursor database");
    assert!(
        CursorIdeAdapter::with_root(&cursor)
            .list_sessions(None)
            .is_err()
    );

    let antigravity = temporary.path().join("antigravity");
    fs::create_dir_all(&antigravity).expect("Antigravity root");
    fs::write(
        antigravity.join("conversation_summaries.db"),
        b"not a sqlite database",
    )
    .expect("corrupt Antigravity database");
    assert!(
        AntigravityAdapter::with_root(&antigravity)
            .list_sessions(None)
            .is_err()
    );

    let hermes = temporary.path().join("hermes");
    fs::create_dir_all(hermes.join("state.db")).expect("directory posing as Hermes database");
    assert!(
        HermesAdapter::with_root(&hermes)
            .list_sessions(None)
            .is_err()
    );
}
