//! Missing provider binaries and stores are normal. Only real failures may warn.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use omnis_adapters::{
    AdapterRegistry, AntigravityAdapter, CursorIdeAdapter, HermesAdapter, ProviderAdapter,
};
use omnis_ir::Provider;

const SCENARIO: &str = "OMNISESSION_ADAPTER_TEST_SCENARIO";

/// Provider binary overrides and PATH names. Children never inherit the caller's overrides.
const BINARY_OVERRIDES: [(&str, &str); 9] = [
    ("OMNI_CLAUDE_BIN", "claude"),
    ("OMNI_CODEX_BIN", "codex"),
    ("OMNI_OPENCODE_BIN", "opencode"),
    ("OMNI_GROK_BIN", "grok"),
    ("OMNI_HERMES_BIN", "hermes"),
    ("OMNI_ANTIGRAVITY_BIN", "agy"),
    ("OMNI_PI_BIN", "pi"),
    ("OMNI_OMP_BIN", "omp"),
    ("OMNI_CURSOR_AGENT_BIN", "cursor-agent"),
];

/// Reruns one test in a child process whose provider environment points into an empty home.
///
/// `prepare` receives the synthetic home and its PATH directory, and returns extra child
/// variables. The child owns its environment, so no test mutates process-global state.
fn run_in_synthetic_home(
    test: &str,
    scenario: &str,
    prepare: impl FnOnce(&Path, &Path) -> Vec<(&'static str, PathBuf)>,
) {
    let home = tempfile::tempdir().expect("synthetic home");
    let root = home.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("synthetic PATH directory");
    let variables = prepare(root, &bin);

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
        ("ANTIGRAVITY_IDE_HOME", ".gemini/antigravity"),
        ("PI_CODING_AGENT_DIR", ".pi/agent"),
        ("PI_CODING_AGENT_SESSION_DIR", ".pi/agent/sessions"),
        ("OMP_SESSION_DIR", ".omp/agent/sessions"),
        ("CURSOR_AGENT_HOME", ".cursor/chats"),
        ("CURSOR_CONFIG_DIR", ".cursor"),
        ("CURSOR_IDE_HOME", "Cursor/User"),
    ] {
        command.env(name, root.join(relative));
    }
    for (name, _) in BINARY_OVERRIDES {
        command.env_remove(name);
    }
    for (name, value) in variables {
        command.env(name, value);
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

/// Writes a shell script that appends its arguments to `marker`, then runs `body`.
#[cfg(unix)]
fn write_fake_binary(path: &Path, marker: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(path.parent().expect("fake binary directory"))
        .expect("fake binary directory");
    fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
            marker.display()
        ),
    )
    .expect("fake binary");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable fake binary");
}

#[test]
fn every_provider_lists_an_empty_home_without_warnings() {
    if !in_child() {
        run_in_synthetic_home(
            "every_provider_lists_an_empty_home_without_warnings",
            "empty-home",
            |_, _| Vec::new(),
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
    assert_eq!(listed.len(), 11, "{listed:?}");
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
            |_, bin| {
                let opencode = bin.join("opencode");
                fs::write(&opencode, "#!/bin/sh\n").expect("synthetic OpenCode");
                fs::set_permissions(&opencode, fs::Permissions::from_mode(0o644))
                    .expect("non-executable OpenCode");
                Vec::new()
            },
        );
        return;
    }

    let error = AdapterRegistry::with_local_adapters()
        .list_sessions_with_notes(Provider::OpenCode, None)
        .expect_err("non-executable OpenCode must warn");
    assert!(error.to_string().contains("opencode"), "{error:#}");
}

#[cfg(unix)]
#[test]
fn opencode_override_runs_instead_of_path_binary() {
    use omnis_adapters::installed_opencode_model;
    use omnis_ir::SessionRef;

    if !in_child() {
        run_in_synthetic_home(
            "opencode_override_runs_instead_of_path_binary",
            "opencode-override",
            |root, bin| {
                let markers = root.join("markers");
                fs::create_dir_all(&markers).expect("marker directory");
                write_fake_binary(
                    &bin.join("opencode"),
                    &markers.join("path-opencode"),
                    "printf '[]'",
                );
                let custom = root.join("custom/opencode-build");
                write_fake_binary(
                    &custom,
                    &markers.join("override-opencode"),
                    r#"case "$2" in models) printf 'fake/model\n' ;; *) printf '[{"id":"ses_override"}]' ;; esac"#,
                );
                // The CLI accepts a Cursor Agent override only when it names `cursor-agent`.
                let cursor = root.join("custom/agent-build");
                write_fake_binary(&cursor, &markers.join("override-cursor"), "exit 0");
                vec![
                    ("OMNI_OPENCODE_BIN", custom),
                    ("OMNI_CURSOR_AGENT_BIN", cursor),
                ]
            },
        );
        return;
    }

    let home = PathBuf::from(env::var_os("HOME").expect("synthetic HOME"));
    let custom = fs::canonicalize(home.join("custom/opencode-build")).expect("override binary");
    let registry = AdapterRegistry::with_local_adapters();
    let adapter = registry
        .adapter(Provider::OpenCode)
        .expect("OpenCode adapter");

    let installation = adapter.probe();
    assert!(installation.installed);
    assert_eq!(installation.executable, Some(custom));
    let sessions = adapter.list_sessions(None).expect("override session list");
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session.id.as_str())
            .collect::<Vec<_>>(),
        ["ses_override"]
    );
    adapter
        .read_session(&SessionRef::new(Provider::OpenCode, "ses_override"))
        .expect("override export");
    assert_eq!(
        installed_opencode_model(&home).expect("override model list"),
        ("fake".to_owned(), "model".to_owned())
    );
    assert_eq!(
        registry
            .adapter(Provider::CursorCli)
            .expect("Cursor Agent adapter")
            .probe()
            .executable,
        None
    );

    assert!(
        !home.join("markers/path-opencode").exists(),
        "PATH `opencode` ran"
    );
    let calls = fs::read_to_string(home.join("markers/override-opencode")).expect("override calls");
    for expected in [
        "--pure session list --format json",
        "--pure export ses_override",
        "--pure models",
    ] {
        assert!(
            calls.lines().any(|line| line == expected),
            "override did not run `{expected}`: {calls:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn invalid_binary_overrides_mean_not_installed_without_path_fallback() {
    use std::os::unix::fs::PermissionsExt;

    use omnis_adapters::installed_opencode_model;
    use omnis_ir::SessionRef;

    if !in_child() {
        for case in ["missing", "relative", "not-executable"] {
            run_in_synthetic_home(
                "invalid_binary_overrides_mean_not_installed_without_path_fallback",
                case,
                |root, bin| {
                    let markers = root.join("markers");
                    fs::create_dir_all(&markers).expect("marker directory");
                    BINARY_OVERRIDES
                        .into_iter()
                        .map(|(variable, name)| {
                            write_fake_binary(&bin.join(name), &markers.join(name), "printf '[]'");
                            let value = match case {
                                "missing" => root.join("missing").join(name),
                                "relative" => PathBuf::from(name),
                                _ => {
                                    let path = root.join("not-executable").join(name);
                                    write_fake_binary(
                                        &path,
                                        &markers.join(format!("{name}-override")),
                                        "printf '[]'",
                                    );
                                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                                        .expect("non-executable override");
                                    path
                                }
                            };
                            (variable, value)
                        })
                        .collect()
                },
            );
        }
        return;
    }

    let home = PathBuf::from(env::var_os("HOME").expect("synthetic HOME"));
    let registry = AdapterRegistry::with_local_adapters();
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Grok,
        Provider::Hermes,
        Provider::Antigravity,
        Provider::Pi,
        Provider::CursorCli,
    ] {
        let installation = registry
            .adapter(provider)
            .expect("registered adapter")
            .probe();
        assert_eq!(
            installation.executable, None,
            "{provider} fell back to PATH"
        );
    }

    let adapter = registry
        .adapter(Provider::OpenCode)
        .expect("OpenCode adapter");
    assert!(!adapter.probe().installed);
    let (sessions, notes) = registry
        .list_sessions_with_notes(Provider::OpenCode, None)
        .expect("invalid OpenCode override lists quietly");
    assert!(sessions.is_empty(), "{sessions:?}");
    assert!(notes.is_empty(), "{notes:?}");
    assert!(
        adapter
            .read_session(&SessionRef::new(Provider::OpenCode, "ses_any"))
            .is_err()
    );
    assert!(installed_opencode_model(&home).is_err());
    let ran = fs::read_dir(home.join("markers"))
        .expect("marker directory")
        .map(|entry| entry.expect("marker entry").file_name())
        .collect::<Vec<_>>();
    assert!(ran.is_empty(), "provider binaries ran: {ran:?}");
}
