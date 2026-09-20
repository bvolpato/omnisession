//! Permission modes a target agent can start in, chosen on the target page or with `--mode`.
//!
//! Flags come from each agent's own `--help` or official CLI reference. An installed version may
//! predate a flag, so a mode is used only when that binary's `--help` lists it.

use std::{fs, path::Path, process::Stdio, time::UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use omnis_ir::Provider;
use serde_json::{Value, json};

use crate::interrupt::{HelperProcess, wait_or_kill};

/// How much the agent may do without asking, from least to most.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
pub(crate) enum ModeKind {
    /// No flags: the agent's own settings decide.
    Default,
    /// Edits apply without asking; commands still ask.
    AcceptEdits,
    /// Safe actions run on their own; risky ones still ask.
    Auto,
    /// Nothing asks; every command runs.
    Yolo,
}

impl ModeKind {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub(crate) const fn summary(self) -> &'static str {
        match self {
            Self::Default => "the agent's own settings decide",
            Self::AcceptEdits => "edits apply without asking; commands still ask",
            Self::Auto => "safe actions run on their own; risky ones still ask",
            Self::Yolo => "nothing asks; every command runs",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LaunchMode {
    pub(crate) kind: ModeKind,
    /// Placed before the agent's own launch arguments.
    pub(crate) args: &'static [&'static str],
    /// Words the agent's `--help` must contain for this mode to exist in that version.
    help_words: &'static [&'static str],
}

impl LaunchMode {
    /// The flags as typed.
    pub(crate) fn flags(&self) -> String {
        if self.args.is_empty() {
            "no flags".to_owned()
        } else {
            self.args.join(" ")
        }
    }
}

const fn mode(
    kind: ModeKind,
    args: &'static [&'static str],
    help_words: &'static [&'static str],
) -> LaunchMode {
    LaunchMode {
        kind,
        args,
        help_words,
    }
}

const DEFAULT: LaunchMode = mode(ModeKind::Default, &[], &[]);

const CLAUDE: &[LaunchMode] = &[
    DEFAULT,
    mode(
        ModeKind::AcceptEdits,
        &["--permission-mode", "acceptEdits"],
        &["--permission-mode", "acceptEdits"],
    ),
    mode(
        ModeKind::Auto,
        &["--permission-mode", "auto"],
        &["--permission-mode", "auto"],
    ),
    mode(
        ModeKind::Yolo,
        &["--dangerously-skip-permissions"],
        &["--dangerously-skip-permissions"],
    ),
];
const CODEX: &[LaunchMode] = &[
    DEFAULT,
    mode(ModeKind::Auto, &["--approve-for-me"], &["--approve-for-me"]),
    mode(
        ModeKind::Yolo,
        &["--dangerously-bypass-approvals-and-sandbox"],
        &["--dangerously-bypass-approvals-and-sandbox"],
    ),
];
// Grok's reference documents the flag, not whether `--help` lists each value.
const GROK: &[LaunchMode] = &[
    DEFAULT,
    mode(
        ModeKind::AcceptEdits,
        &["--permission-mode", "acceptEdits"],
        &["--permission-mode"],
    ),
    mode(
        ModeKind::Auto,
        &["--permission-mode", "auto"],
        &["--permission-mode"],
    ),
    mode(ModeKind::Yolo, &["--always-approve"], &["--always-approve"]),
];
const CURSOR_AGENT: &[LaunchMode] = &[
    DEFAULT,
    mode(ModeKind::Auto, &["--auto-review"], &["--auto-review"]),
    mode(ModeKind::Yolo, &["--force"], &["--force"]),
];
const ANTIGRAVITY: &[LaunchMode] = &[
    DEFAULT,
    mode(
        ModeKind::AcceptEdits,
        &["--mode", "accept-edits"],
        &["--mode", "accept-edits"],
    ),
    mode(
        ModeKind::Yolo,
        &["--dangerously-skip-permissions"],
        &["--dangerously-skip-permissions"],
    ),
];
const OPENCODE: &[LaunchMode] = &[DEFAULT, mode(ModeKind::Yolo, &["--auto"], &["--auto"])];
const HERMES: &[LaunchMode] = &[DEFAULT, mode(ModeKind::Yolo, &["--yolo"], &["--yolo"])];

/// Modes from least to most permissive. Empty when the agent has no permission prompts to
/// configure (Pi runs every tool) or no launcher that takes flags (the IDEs).
pub(crate) const fn modes(provider: Provider) -> &'static [LaunchMode] {
    match provider {
        Provider::Claude => CLAUDE,
        Provider::Codex => CODEX,
        Provider::Grok => GROK,
        Provider::CursorCli => CURSOR_AGENT,
        Provider::Antigravity => ANTIGRAVITY,
        Provider::OpenCode => OPENCODE,
        Provider::Hermes => HERMES,
        Provider::Pi
        | Provider::CursorIde
        | Provider::AntigravityIde
        | Provider::GenericAcp
        | Provider::Imported => &[],
    }
}

/// The mode an agent starts in when nothing was chosen: its own default, with no flags passed.
/// Auto-approval and yolo are choices the user makes. `OMNI_MODE` names a standing preference,
/// which applies where the agent has that mode.
pub(crate) fn default_mode(provider: Provider) -> Option<&'static LaunchMode> {
    let modes = modes(provider);
    let preferred = preferred_kind();
    modes
        .iter()
        .find(|mode| mode.kind == preferred)
        .or_else(|| modes.first())
}

fn preferred_kind() -> ModeKind {
    // Unit tests pin the built-in default instead of a developer's environment.
    #[cfg(not(test))]
    if let Some(kind) = std::env::var("OMNI_MODE")
        .ok()
        .and_then(|value| ModeKind::from_str(value.trim(), true).ok())
    {
        return kind;
    }
    ModeKind::Default
}

pub(crate) fn find_mode(provider: Provider, kind: ModeKind) -> Option<&'static LaunchMode> {
    modes(provider).iter().find(|mode| mode.kind == kind)
}

/// What [`resolve`] decided, for the launch announcement.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ResolvedMode {
    pub(crate) mode: &'static LaunchMode,
    /// Set when the installed agent lacks the wanted mode and a lower one runs instead.
    pub(crate) downgraded_from: Option<ModeKind>,
}

/// Picks the mode to launch `provider` in. `requested` is the target page or `--mode` choice;
/// without one the agent's own default applies. `supported` reports whether the installed
/// agent has a mode, and a missing mode steps down to the nearest lower one it has.
///
/// # Errors
///
/// Returns an error when `requested` names a mode the agent never has.
pub(crate) fn resolve(
    provider: Provider,
    requested: Option<ModeKind>,
    mut supported: impl FnMut(&LaunchMode) -> bool,
) -> Result<Option<ResolvedMode>> {
    let modes = modes(provider);
    let Some(wanted) = (match requested {
        None => default_mode(provider),
        Some(kind) => {
            let found = find_mode(provider, kind);
            if found.is_none() && !modes.is_empty() {
                bail!(
                    "{provider} has no `{}` mode; available: {}",
                    kind.id(),
                    modes
                        .iter()
                        .map(|mode| mode.kind.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            found
        }
    }) else {
        return Ok(None);
    };
    let mode = modes
        .iter()
        .rev()
        .filter(|mode| mode.kind <= wanted.kind)
        .find(|mode| mode.args.is_empty() || supported(mode))
        .unwrap_or(&DEFAULT);
    Ok(Some(ResolvedMode {
        mode,
        downgraded_from: (mode.kind != wanted.kind).then_some(wanted.kind),
    }))
}

/// Whether `help` lists the mode: its flag as a whole word, and every further word (a value such
/// as `auto`) inside that flag's own entry, which runs until the next line that starts an option.
/// A value that only appears under another option, like `--autocompact <auto|tokens>`, is no
/// evidence.
fn help_lists(help: &str, mode: &LaunchMode) -> bool {
    fn words(line: &str) -> impl Iterator<Item = &str> {
        line.split(|character: char| {
            character.is_whitespace()
                || matches!(
                    character,
                    ',' | '(' | ')' | '[' | ']' | '<' | '>' | '|' | '"' | '\'' | '=' | ':'
                )
        })
    }
    let Some((flag, values)) = mode.help_words.split_first() else {
        return true;
    };
    let lines = help.lines().collect::<Vec<_>>();
    lines.iter().enumerate().any(|(index, line)| {
        if !line.trim_start().starts_with('-') || !words(line).any(|word| word == *flag) {
            return false;
        }
        let entry = lines[index..]
            .iter()
            .enumerate()
            .take_while(|(offset, line)| *offset == 0 || !line.trim_start().starts_with('-'))
            .flat_map(|(_, line)| words(line))
            .collect::<Vec<_>>();
        values.iter().all(|value| entry.contains(value))
    })
}

const MAX_CACHED_BINARIES: usize = 64;

/// Modes the installed `binary` lists in `--help`. The answer is cached per binary until the file
/// changes, because a provider's `--help` can take two seconds.
pub(crate) struct InstalledModes {
    supported: Vec<ModeKind>,
}

impl InstalledModes {
    pub(crate) fn probe(provider: Provider, binary: &Path) -> Self {
        let cache_path = omnis_store::state_root()
            .ok()
            .map(|root| root.join("cache").join("launch-modes.json"));
        Self::probe_with_cache(provider, binary, cache_path.as_deref())
    }

    fn probe_with_cache(provider: Provider, binary: &Path, cache_path: Option<&Path>) -> Self {
        let identity = binary_identity(binary);
        let mut cache = cache_path.map_or_else(|| json!({}), read_cache);
        if let Some((key, stamp)) = &identity
            && let Some(cached) = cache.get(key)
            && cached["stamp"] == *stamp
            && let Some(kinds) = cached["supported"].as_array()
        {
            return Self {
                supported: kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|id| ModeKind::from_str(id, false).ok())
                    .collect(),
            };
        }
        // A probe that timed out or failed says nothing about the binary, so it is never cached.
        let Ok(help) = help_output(binary) else {
            return Self {
                supported: Vec::new(),
            };
        };
        let supported = modes(provider)
            .iter()
            .filter(|mode| !mode.args.is_empty() && help_lists(&help, mode))
            .map(|mode| mode.kind)
            .collect::<Vec<_>>();
        if let (Some((key, stamp)), Some(path), Some(entries)) =
            (identity, cache_path, cache.as_object_mut())
        {
            // Every agent update adds a path, so start over instead of growing without bound.
            if entries.len() >= MAX_CACHED_BINARIES {
                entries.clear();
            }
            let kinds = supported.iter().map(|kind| kind.id()).collect::<Vec<_>>();
            entries.insert(key, json!({ "stamp": stamp, "supported": kinds }));
            // Best effort: a missing cache only costs the next launch another probe.
            let _ = write_cache(path, &cache);
        }
        Self { supported }
    }

    pub(crate) fn has(&self, mode: &LaunchMode) -> bool {
        self.supported.contains(&mode.kind)
    }
}

/// Canonical path plus a stamp that changes whenever the file is replaced.
fn binary_identity(binary: &Path) -> Option<(String, String)> {
    let canonical = fs::canonicalize(binary).ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    // The omni version is part of the stamp, so a release that changes a mode table asks again.
    Some((
        canonical.to_str()?.to_owned(),
        format!(
            "{modified}:{}:{}",
            metadata.len(),
            env!("CARGO_PKG_VERSION")
        ),
    ))
}

fn read_cache(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_cache(path: &Path, cache: &Value) -> Result<()> {
    let directory = path
        .parent()
        .context("launch mode cache has no directory")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, cache)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn help_output(binary: &Path) -> Result<String> {
    const MAX_HELP_BYTES: u64 = 1024 * 1024;
    let output = tempfile::NamedTempFile::new().context("creating help output buffer")?;
    // One open file description, so stderr appends after stdout instead of overwriting it.
    let writer = output.reopen()?;
    let mut child = crate::shim::provider_process(binary)?
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer))
        .outside_terminal_group()
        .spawn()
        .with_context(|| format!("executing `{}`", binary.display()))?;
    if wait_or_kill(&mut child, crate::version_gate::PROBE_TIMEOUT)?.is_none() {
        bail!("`--help` timed out");
    }
    if output.as_file().metadata()?.len() > MAX_HELP_BYTES {
        bail!("`--help` output exceeds safe limit");
    }
    fs::read_to_string(output.path()).context("reading `--help` output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_default_passes_no_flags_and_stronger_modes_are_a_choice() {
        for &provider in Provider::ALL {
            let Some(default) = default_mode(provider) else {
                assert!(modes(provider).is_empty(), "{provider}");
                continue;
            };
            assert_eq!(default.kind, ModeKind::Default, "{provider}");
            assert!(default.args.is_empty(), "{provider}");
        }
        assert_eq!(default_mode(Provider::Pi), None);
        // Left to right runs from least to most permissive, so yolo is always the last step.
        for &provider in Provider::ALL {
            let modes = modes(provider);
            assert!(modes.iter().map(|mode| mode.kind).is_sorted(), "{provider}");
            if let Some(last) = modes.last() {
                assert_eq!(last.kind, ModeKind::Yolo, "{provider}");
            }
        }
    }

    #[test]
    fn missing_mode_steps_down_and_unknown_mode_is_refused() {
        let all = |_: &LaunchMode| true;
        let resolved = resolve(Provider::Codex, None, all)
            .expect("default")
            .expect("Codex has modes");
        assert!(resolved.mode.args.is_empty());
        assert_eq!(resolved.downgraded_from, None);
        let resolved = resolve(Provider::Codex, Some(ModeKind::Auto), all)
            .expect("auto")
            .expect("Codex has modes");
        assert_eq!(resolved.mode.args, ["--approve-for-me"]);

        // An older Claude without `auto` still has `acceptEdits`.
        let no_auto = |mode: &LaunchMode| mode.kind != ModeKind::Auto;
        let resolved = resolve(Provider::Claude, Some(ModeKind::Auto), no_auto)
            .expect("auto")
            .expect("Claude has modes");
        assert_eq!(resolved.mode.kind, ModeKind::AcceptEdits);
        assert_eq!(resolved.downgraded_from, Some(ModeKind::Auto));

        let none = |_: &LaunchMode| false;
        let resolved = resolve(Provider::Claude, Some(ModeKind::Yolo), none)
            .expect("yolo")
            .expect("Claude has modes");
        assert_eq!(resolved.mode.kind, ModeKind::Default);
        assert!(resolved.mode.args.is_empty());

        let error = resolve(Provider::Codex, Some(ModeKind::AcceptEdits), all)
            .expect_err("Codex has no accept-edits mode");
        assert!(
            error.to_string().contains("available: default, auto, yolo"),
            "{error}"
        );
        assert_eq!(
            resolve(Provider::Pi, Some(ModeKind::Yolo), all).ok(),
            Some(None)
        );
    }

    #[test]
    fn help_words_match_whole_words_only() {
        let claude = r#"  --permission-mode <mode>  (choices: "acceptEdits", "auto", "plan")
  --autocompact <auto|tokens>"#;
        let auto = find_mode(Provider::Claude, ModeKind::Auto).expect("Claude auto");
        assert!(help_lists(claude, auto));
        // An older Claude: `auto` appears only under another option, which is no evidence.
        let older = r#"  --autocompact <auto|tokens>  Auto-compact window size (auto, or tokens)
  --permission-mode <mode>  Permission mode to use for the session
                            (choices: "acceptEdits",
                            "plan")
  --print  Print (auto) and exit"#;
        assert!(!help_lists(older, auto));
        let wrapped = r#"  --permission-mode <mode>  Permission mode to use for the session
                            (choices: "acceptEdits", "auto",
                            "plan")
  --print"#;
        assert!(help_lists(wrapped, auto));
        let yolo = find_mode(Provider::OpenCode, ModeKind::Yolo).expect("OpenCode yolo");
        assert!(help_lists(
            "      --auto          auto-approve permissions",
            yolo
        ));
        assert!(!help_lists("      --autoupdate    update on start", yolo));
    }

    #[cfg(unix)]
    #[test]
    fn installed_modes_come_from_help_and_are_cached_until_the_binary_changes() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let binary = temporary.path().join("codex");
        let calls = temporary.path().join("calls");
        let script = |help: &str| {
            format!(
                "#!/bin/sh\necho called >> '{}'\necho '{help}'\n",
                calls.display()
            )
        };
        let write = |help: &str| {
            fs::write(&binary, script(help)).expect("launcher");
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("launcher mode");
            // Linux refuses to run a program another test's fork still holds open for writing.
            crate::test_support::output_after_write(
                std::process::Command::new(&binary).arg("--version"),
            );
            fs::remove_file(&calls).expect("reset call log");
        };
        write("  --approve-for-me  Route approvals");
        let cache = temporary.path().join("launch-modes.json");
        let auto = find_mode(Provider::Codex, ModeKind::Auto).expect("Codex auto");
        let yolo = find_mode(Provider::Codex, ModeKind::Yolo).expect("Codex yolo");

        for _ in 0..2 {
            let installed =
                InstalledModes::probe_with_cache(Provider::Codex, &binary, Some(&cache));
            assert!(installed.has(auto));
            assert!(!installed.has(yolo));
        }
        let call_count = || fs::read_to_string(&calls).map_or(0, |log| log.lines().count());
        assert_eq!(call_count(), 1, "second answer must come from the cache");

        write("  --approve-for-me\n  --dangerously-bypass-approvals-and-sandbox  longer help");
        let installed = InstalledModes::probe_with_cache(Provider::Codex, &binary, Some(&cache));
        assert!(installed.has(yolo));
        assert_eq!(call_count(), 1, "a changed binary is probed again");

        // A probe that cannot run says nothing about the binary, so the next launch asks again.
        let broken = temporary.path().join("broken");
        fs::write(&broken, "#!/nonexistent/interpreter\n").expect("broken launcher");
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o755)).expect("launcher mode");
        let installed = InstalledModes::probe_with_cache(Provider::Codex, &broken, Some(&cache));
        assert!(!installed.has(auto));
        let cached = fs::read_to_string(&cache).expect("cache file");
        assert!(
            !cached.contains("broken"),
            "failed probe was cached: {cached}"
        );
    }
}
