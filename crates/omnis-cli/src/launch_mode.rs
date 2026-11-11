//! Permission modes a target agent can start in, chosen on the target page or with `--mode`.
//!
//! Flags come from each agent's own `--help` or official CLI reference. An installed version may
//! predate a flag, so a mode is used only when that binary's `--help` lists it.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::Path,
    process::{Command, Stdio},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use omnis_ir::Provider;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{LaunchPlan, launch_profile::AgentTarget};

/// How much the agent may do without asking, from least to most.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
pub(crate) enum ModeKind {
    /// No flags: the agent's own settings decide.
    Default,
    /// Reads run without asking; edits and commands ask.
    AlwaysAsk,
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
            Self::AlwaysAsk => "always-ask",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub(crate) const fn summary(self) -> &'static str {
        match self {
            Self::Default => "the agent's own settings decide",
            Self::AlwaysAsk => "reads run; edits and commands ask",
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
const OMP: &[LaunchMode] = &[
    DEFAULT,
    mode(
        ModeKind::AlwaysAsk,
        &["--approval-mode", "always-ask"],
        &["--approval-mode", "always-ask"],
    ),
    mode(
        ModeKind::AcceptEdits,
        &["--approval-mode", "write"],
        &["--approval-mode", "write"],
    ),
    mode(
        ModeKind::Yolo,
        &["--approval-mode", "yolo"],
        &["--approval-mode", "yolo"],
    ),
];

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
        Provider::OhMyPi => OMP,
        Provider::Pi
        | Provider::CursorIde
        | Provider::AntigravityIde
        | Provider::AntigravityDesktop
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
    mut supported: impl FnMut(&'static LaunchMode) -> bool,
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
    if wanted.kind == ModeKind::AlwaysAsk && mode.kind != wanted.kind {
        bail!(
            "{provider} cannot validate `always-ask`; refusing to use the harness default, which may allow yolo"
        );
    }
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
    let mut in_entry = false;
    let mut found = vec![false; values.len()];
    let mut option_indent = None;
    let mut long_alias_indent = None;
    let mut description_column = usize::MAX;
    let mut table_description = None;
    for line in help.lines() {
        let text = line.trim_start();
        if text.is_empty() {
            continue;
        }
        let leading = line.len() - text.len();
        let indent = help_column(&line[..leading]);
        let option = text
            .strip_prefix('-')
            .filter(|name| name.starts_with(|character: char| !character.is_whitespace()));
        if let Some((declaration, description)) =
            option.map(|_| option_columns(line, leading, table_description))
            && (indent < description_column
                || (long_alias_indent == Some(indent)
                    && text.starts_with("--")
                    && description.is_none_or(|column| {
                        let gap = &text[declaration.len()..column];
                        gap.len() > 1 || gap.contains('\t')
                    })))
        {
            in_entry = declaration.split_whitespace().any(|field| {
                for item in field.split([',', '|']) {
                    if let Some((name, _)) = item.split_once(['=', '<', '[', ':']) {
                        if name == *flag {
                            return true;
                        }
                        // An unquoted option value can itself contain commas and pipes.
                        if !item.ends_with(['>', ']']) {
                            break;
                        }
                    } else if item == *flag {
                        return true;
                    }
                }
                false
            });
            found.fill(false);
            option_indent = Some(indent);
            // Short options often appear as `-x, --example`. A long-only row can
            // start in that same column, even after a short description.
            long_alias_indent = (!text.starts_with("--")).then_some(indent + "-x, ".len());
            description_column =
                description.map_or(usize::MAX, |byte| help_column(&line[..leading + byte]));
            if !text.starts_with("--")
                && declaration
                    .find("--")
                    .is_some_and(|offset| declaration[..offset].ends_with([',', '|', ' ', '\t']))
                && description.is_some_and(|offset| is_column_gap(&text[declaration.len()..offset]))
            {
                table_description = Some(description_column);
            }
        } else if option_indent.is_some_and(|previous| indent > previous) {
            description_column = description_column.min(indent);
        } else {
            in_entry = false;
            option_indent = None;
            long_alias_indent = None;
            description_column = usize::MAX;
            table_description = None;
        }
        if in_entry {
            for (value, seen) in values.iter().zip(&mut found) {
                *seen |= words(text).any(|word| word == *value);
            }
            if found.iter().all(|seen| *seen) {
                return true;
            }
        }
    }
    false
}

fn help_column(prefix: &str) -> usize {
    prefix.chars().fold(0, |column, character| {
        if character == '\t' {
            column + (8 - column % 8)
        } else {
            column + unicode_width::UnicodeWidthChar::width(character).unwrap_or(0)
        }
    })
}

/// Separates an option's names and arguments from its description. A short and a long
/// alias can each have a required or optional argument, as in `-p MODE, --policy MODE`.
fn option_columns(
    line: &str,
    leading: usize,
    table_description: Option<usize>,
) -> (&str, Option<usize>) {
    let text = &line[leading..];
    let mut end = 0;
    let mut seen_long_name = false;
    for field in text.split_whitespace() {
        let rest = &text[end..];
        let column = text.len() - rest.trim_start().len();
        let prefix = &text[..end];
        let after_field = &text[column + field.len()..];
        let argument = field.split([',', '|']).next().unwrap_or(field);
        // Some help formatters print a lowercase argument for both forms, as in
        // `-p mode  --policy mode`. The repeated argument identifies the declaration.
        let mut following = after_field.split_whitespace();
        let repeated_argument = !seen_long_name
            && !prefix.contains(char::is_whitespace)
            && following
                .next()
                .is_some_and(|alias| alias.starts_with("--"))
            && following.next() == Some(argument);
        let is_argument = repeated_argument || is_metavar(argument) || field.ends_with([',', '|']);
        let padded = is_column_gap(&text[end..column]);
        // An unpunctuated alias needs a declaration of its own. A description such
        // as `--auto is deprecated` does not establish that the program accepts it.
        let padded_alias = !seen_long_name
            && if field.starts_with("--") {
                let next = after_field.trim_start();
                let next_field = next.split_whitespace().next();
                let repeated = next_field.is_some_and(|value| {
                    !value.starts_with('-') && prefix.split_whitespace().next_back() == Some(value)
                });
                let in_declarations = table_description
                    .is_none_or(|stop| help_column(&line[..leading + column]) < stop);
                repeated
                    || (in_declarations
                        && (next.is_empty()
                            || is_column_gap(&after_field[..after_field.len() - next.len()])
                            || next_field.is_some_and(is_metavar)
                            || field.contains(['=', '<', '['])))
            } else {
                is_argument && after_field.trim_start().starts_with("--")
            };
        if (padded && !prefix.ends_with([',', '|']) && !padded_alias)
            || !(field.starts_with('-') || is_argument)
        {
            return (&text[..end], Some(column));
        }
        seen_long_name |= field.contains("--");
        end = column + field.len();
    }
    (&text[..end], None)
}

fn is_metavar(field: &str) -> bool {
    field.starts_with(['<', '[', '{'])
        || (!field.is_empty()
            && field.bytes().all(|byte| {
                byte.is_ascii_uppercase() || byte.is_ascii_digit() || b"_-.".contains(&byte)
            }))
}

fn is_column_gap(whitespace: &str) -> bool {
    whitespace.len() > 1 || whitespace.contains('\t')
}

const MAX_CACHED_BINARIES: usize = 64;

/// Permission modes reported by the launcher in its selected workspace and environment.
pub(crate) struct InstalledModes {
    supported: Vec<ModeKind>,
}

impl InstalledModes {
    pub(crate) fn probe(
        target: AgentTarget,
        project: &Path,
        requested: &'static LaunchMode,
    ) -> Result<Self> {
        let program = target
            .provider
            .command()
            .context("agent has no permission-mode launcher")?;
        let plan = target.launch(LaunchPlan {
            program: program.to_owned(),
            args: vec!["--help".to_owned()],
            cwd: Some(project.to_owned()),
            env: Vec::new(),
        })?;
        let mut command = crate::prepare_launch_command(&plan)?;
        // A replacement program can delegate to a different runtime without changing itself.
        let cache_path = if target.overrides_program() {
            None
        } else {
            omnis_store::state_root()
                .ok()
                .map(|root| root.join("cache").join("launch-modes.json"))
        };
        Ok(Self::probe_with_cache(
            target.provider,
            &mut command,
            requested,
            cache_path.as_deref(),
            target.profile.is_some(),
        ))
    }

    fn probe_with_cache(
        provider: Provider,
        command: &mut Command,
        requested: &'static LaunchMode,
        cache_path: Option<&Path>,
        profiled: bool,
    ) -> Self {
        let identity = cache_path.and_then(|_| invocation_identity(command, profiled));
        let specification = json!({
            "option_declarations": modes(provider)
                .iter()
                .map(|mode| (mode.kind.id(), mode.help_words))
                .collect::<Vec<_>>()
        });
        let mut cache = cache_path.map_or_else(|| json!({}), read_cache);
        if let Some(key) = &identity
            && let Some(cached) = cache.get(key)
            && cached["specification"] == specification
            && let Some(kinds) = cached["verified_modes"].as_array()
        {
            return Self {
                supported: kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|id| ModeKind::from_str(id, false).ok())
                    .collect(),
            };
        }
        // If the launcher failed, the next launch should retry the probe.
        let Ok((help, complete)) = help_output(command, requested) else {
            return Self {
                supported: Vec::new(),
            };
        };
        let supported = modes(provider)
            .iter()
            .filter(|mode| !mode.args.is_empty() && help_lists(&help, mode))
            .map(|mode| mode.kind)
            .collect::<Vec<_>>();
        // A worker can emit more options after the requested mode appears. Cache only complete help.
        if complete
            && let (Some(key), Some(path), Some(entries)) =
                (identity, cache_path, cache.as_object_mut())
        {
            // Discard old invocations when the cache is full.
            if entries.len() >= MAX_CACHED_BINARIES {
                entries.clear();
            }
            let kinds = supported.iter().map(|kind| kind.id()).collect::<Vec<_>>();
            entries.insert(
                key,
                json!({ "specification": specification, "verified_modes": kinds }),
            );
            // Best effort: a missing cache only costs the next launch another probe.
            let _ = write_cache(path, &cache);
        }
        Self { supported }
    }

    pub(crate) fn has(&self, mode: &LaunchMode) -> bool {
        self.supported.contains(&mode.kind)
    }
}

/// Hash the effective invocation. Profile arguments and environment values can contain secrets.
fn invocation_identity(command: &Command, profiled: bool) -> Option<String> {
    let mut digest = Sha256::new();
    hash_value(&mut digest, b'B', OsStr::new(env!("CARGO_PKG_VERSION")));
    let program = Path::new(command.get_program());
    stamp_executable(&mut digest, program)?;
    let cwd = command
        .get_current_dir()
        .map_or_else(|| env::current_dir().ok(), |path| Some(path.to_owned()))?;
    hash_value(&mut digest, b'D', cwd.as_os_str());
    let mut environment: BTreeMap<OsString, OsString> = env::vars_os()
        .map(|(name, value)| (env_name(&name), value))
        .collect();
    for (name, value) in command.get_envs() {
        let name = env_name(name);
        if let Some(value) = value {
            environment.insert(name, value.to_owned());
        } else {
            environment.remove(&name);
        }
    }
    #[cfg(unix)]
    stamp_interpreter(&mut digest, program, profiled, &environment, &cwd)?;
    // The validated Windows npm shim runs node.exe with the provider script as its first argument.
    #[cfg(windows)]
    if let Some(script) = command
        .get_args()
        .next()
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        if profiled {
            return None;
        }
        stamp_executable(&mut digest, script)?;
    }
    for argument in command.get_args() {
        hash_value(&mut digest, b'A', argument);
    }
    for (name, value) in environment {
        hash_value(&mut digest, b'N', &name);
        hash_value(&mut digest, b'V', &value);
    }
    Some(hex::encode(digest.finalize()))
}

fn env_name(name: &OsStr) -> OsString {
    #[cfg(windows)]
    {
        name.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        name.to_owned()
    }
}

fn hash_value(digest: &mut Sha256, tag: u8, value: &OsStr) {
    let bytes = value.as_encoded_bytes();
    digest.update([tag]);
    digest.update(bytes.len().to_le_bytes());
    digest.update(bytes);
}

fn stamp_executable(digest: &mut Sha256, file: &Path) -> Option<()> {
    let canonical = fs::canonicalize(file).ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    hash_value(digest, b'P', canonical.as_os_str());
    digest.update(metadata.len().to_le_bytes());
    digest.update(modified.as_nanos().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(metadata.dev().to_le_bytes());
        digest.update(metadata.ino().to_le_bytes());
        digest.update(metadata.ctime().to_le_bytes());
        digest.update(metadata.ctime_nsec().to_le_bytes());
    }
    Some(())
}

#[cfg(unix)]
fn stamp_interpreter(
    digest: &mut Sha256,
    program: &Path,
    profiled: bool,
    environment: &BTreeMap<OsString, OsString>,
    cwd: &Path,
) -> Option<()> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    let mut header = [0; 512];
    let size = fs::File::open(program).ok()?.read(&mut header).ok()?;
    let Some(shebang) = header[..size].strip_prefix(b"#!") else {
        return Some(());
    };
    // A profile can change the downstream runtime or settings of a script.
    if profiled {
        return None;
    }
    let end = shebang.iter().position(|&byte| byte == b'\n')?;
    let mut words = shebang[..end]
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty());
    let interpreter = cwd.join(OsStr::from_bytes(words.next()?));
    stamp_executable(digest, &interpreter)?;
    if interpreter.file_name() == Some(OsStr::new("env")) {
        let runtime = OsStr::from_bytes(words.next()?);
        if runtime.as_bytes().starts_with(b"-") || words.next().is_some() {
            return None;
        }
        let selected = if runtime.as_bytes().contains(&b'/') {
            cwd.join(runtime)
        } else {
            env::split_paths(environment.get(OsStr::new("PATH"))?)
                .map(|path| cwd.join(path).join(runtime))
                .find(|path| crate::provider_launch::is_executable(path))?
        };
        stamp_executable(digest, &selected)?;
    }
    Some(())
}

fn read_cache(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|mut cache| cache.get_mut("invocations_v2").map(Value::take))
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_cache(path: &Path, cache: &Value) -> Result<()> {
    let directory = path
        .parent()
        .context("launch mode cache has no directory")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, &json!({ "invocations_v2": cache }))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn help_output(command: &mut Command, requested: &'static LaunchMode) -> Result<(String, bool)> {
    const MAX_HELP_BYTES: usize = 1024 * 1024;
    command.stdin(Stdio::null());
    let (output, complete) = crate::interrupt::with_provider_probes(|| {
        omnis_adapters::bounded_provider_help_output(
            command,
            crate::version_gate::PROBE_TIMEOUT,
            MAX_HELP_BYTES,
            move |help| help_lists(help, requested),
        )
    })
    .context("executing provider permission help")?;
    if !output.status.success() {
        bail!("`--help` exited with {}", output.status);
    }
    let help = String::from_utf8(output.stdout).context("reading `--help` output")?;
    Ok((help, complete))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_defaults_to_no_flags_and_auto_approve_is_not_a_safer_mode() {
        assert_eq!(default_mode(Provider::OhMyPi).unwrap().args, &[] as &[&str]);
        assert!(find_mode(Provider::OhMyPi, ModeKind::Auto).is_none());
        assert_eq!(
            find_mode(Provider::OhMyPi, ModeKind::AlwaysAsk)
                .unwrap()
                .args,
            &["--approval-mode", "always-ask"]
        );
        assert!(resolve(Provider::OhMyPi, Some(ModeKind::AlwaysAsk), |_| false).is_err());
        assert_eq!(
            find_mode(Provider::OhMyPi, ModeKind::AcceptEdits)
                .unwrap()
                .args,
            &["--approval-mode", "write"]
        );
        assert_eq!(
            find_mode(Provider::OhMyPi, ModeKind::Yolo).unwrap().args,
            &["--approval-mode", "yolo"]
        );
    }

    #[test]
    fn omp_advertised_approval_modes_keep_the_requested_mode() {
        let help = "      --plan-yolo                       Auto-approve a plan\n\
                    --auto-approve                    Auto-approve all tool calls (skip approval prompts)\n\
                    --approval-mode=<value>           Override tools.approvalMode for this session (always-ask|write|yolo)";
        for requested in [
            ModeKind::Default,
            ModeKind::AlwaysAsk,
            ModeKind::AcceptEdits,
            ModeKind::Yolo,
        ] {
            let resolved = resolve(Provider::OhMyPi, Some(requested), |mode| {
                help_lists(help, mode)
            })
            .unwrap()
            .unwrap();
            assert_eq!(resolved.mode.kind, requested);
            assert_eq!(resolved.downgraded_from, None);
        }
    }

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
        assert!(help_lists("  -a, --auto    auto-approve", yolo));
        assert!(help_lists("  -y\t--auto    auto-approve", yolo));
        assert!(help_lists("  -x  --auto", yolo));
        assert!(!help_lists("  -x  --auto is deprecated", yolo));
        let aligned = "  -h, --help       show help\n  -x               --auto    is deprecated";
        assert!(!help_lists(aligned, yolo));
        let declared = "  -h, --help       show help\n  -x  --auto       enable auto-approval";
        assert!(help_lists(declared, yolo));
        assert!(!help_lists("  -v  Use with --auto", yolo));
        assert!(!help_lists(
            "  -v, --preview  --auto is a sample flag",
            yolo
        ));
        assert!(!help_lists("  --preview    use with --auto", yolo));
        assert!(!help_lists("  --example=--auto    sample input", yolo));
        let accept = find_mode(Provider::Claude, ModeKind::AcceptEdits).unwrap();
        for declaration in [
            "-p MODE, --permission-mode MODE",
            "-p MODE,--permission-mode MODE",
            "-p  --permission-mode MODE",
            "-p\t--permission-mode MODE",
            "-p MODE   --permission-mode MODE",
            "-p mode   --permission-mode mode",
            "-p <MODE>\t--permission-mode=<MODE>",
            "-p   MODE   --permission-mode MODE",
            "-p <MODE>,   --permission-mode=<MODE>",
            "-p [MODE] | --permission-mode [MODE]",
        ] {
            let help = format!("  {declaration}\n                         Choices: acceptEdits");
            assert!(help_lists(&help, accept), "{declaration}");
        }
        assert!(help_lists(
            "  -v  Give version\n      --permission-mode MODE  Choices: acceptEdits",
            accept,
        ));
        let wrapped_example =
            "  --preview MODE  Use with\n                  --permission-mode acceptEdits";
        assert!(!help_lists(wrapped_example, accept));
        assert!(!help_lists(
            "  --preview MODE\tUse with\n\t\t\t--permission-mode acceptEdits",
            accept,
        ));
        assert!(!help_lists(
            "  -v  Use with\n      --permission-mode acceptEdits",
            accept,
        ));
    }

    #[cfg(unix)]
    fn probe_test_binary(
        provider: Provider,
        binary: &Path,
        requested: &'static LaunchMode,
        cache: &Path,
    ) -> InstalledModes {
        let mut command = Command::new(binary);
        command.arg("--help");
        InstalledModes::probe_with_cache(provider, &mut command, requested, Some(cache), false)
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_help_probe_retries_on_the_next_launch() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("codex");
        let cache = directory.path().join("launch-modes.json");
        fs::write(
            &binary,
            "#!/bin/sh\n[ \"$1\" = --help ] || exit 0\n[ -f \"$0.ready\" ] || exit 1\necho '  --approve-for-me'\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        crate::test_support::output_after_write(
            std::process::Command::new(&binary).arg("--version"),
        );

        let auto = find_mode(Provider::Codex, ModeKind::Auto).unwrap();
        let probe = || probe_test_binary(Provider::Codex, &binary, auto, &cache);
        assert!(!probe().has(auto));
        fs::write(binary.with_extension("ready"), "").unwrap();
        assert!(probe().has(auto));
    }

    #[cfg(unix)]
    #[test]
    fn partial_help_verifies_its_requested_mode_and_leaves_other_modes_for_later_probes() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("claude");
        let cache = temporary.path().join("launch-modes.json");
        fs::write(
            &binary,
            r#"#!/bin/sh
[ "$1" = --help ] || exit 0
printf '  --permission-mode <mode>\n'
if [ -f "$0.second" ]; then
    (sleep 0.1; printf '    (choices: auto)\n' >&2) &
else
    printf '    (choices: acceptEdits)\n' >&2
    sleep 30 &
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        crate::test_support::output_after_write(
            std::process::Command::new(&binary).arg("--version"),
        );

        let accept = find_mode(Provider::Claude, ModeKind::AcceptEdits).unwrap();
        let auto = find_mode(Provider::Claude, ModeKind::Auto).unwrap();
        let probe = |wanted| probe_test_binary(Provider::Claude, &binary, wanted, &cache);
        assert!(probe(accept).has(accept));
        fs::write(binary.with_extension("second"), "").unwrap();
        assert!(probe(auto).has(auto));
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
        fs::write(&cache, br#"{"legacy":{"verified_modes":[]}}"#).unwrap();

        for _ in 0..2 {
            let installed = probe_test_binary(Provider::Codex, &binary, auto, &cache);
            assert!(installed.has(auto));
            assert!(!installed.has(yolo));
        }
        let call_count = || fs::read_to_string(&calls).map_or(0, |log| log.lines().count());
        assert_eq!(call_count(), 1, "second answer must come from the cache");

        write("  --approve-for-me\n  --dangerously-bypass-approvals-and-sandbox  longer help");
        let installed = probe_test_binary(Provider::Codex, &binary, auto, &cache);
        assert!(installed.has(yolo));
        assert_eq!(call_count(), 1, "a changed binary is probed again");

        // A probe that cannot run says nothing about the binary, so the next launch asks again.
        let broken = temporary.path().join("broken");
        fs::write(&broken, "#!/nonexistent/interpreter\n").expect("broken launcher");
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o755)).expect("launcher mode");
        let before = fs::read(&cache).unwrap();
        let installed = probe_test_binary(Provider::Codex, &broken, auto, &cache);
        assert!(!installed.has(auto));
        assert_eq!(
            fs::read(&cache).unwrap(),
            before,
            "failed probe changed cache"
        );
    }
}
