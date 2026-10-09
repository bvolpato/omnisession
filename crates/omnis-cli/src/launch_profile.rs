//! Launch profiles: named ways to start a built-in agent, read from local configuration.
//!
//! A profile adds arguments and environment changes to the launch of one built-in agent, or runs
//! another program in its place. It changes how an agent starts, not where sessions come from:
//! the sessions that a profile starts belong to the agent it names, and `omni` reads them as that
//! agent's sessions. The file lives in the `OmniSession` state directory and is never part of a
//! repository, so settings such as the address of a local gateway stay on one machine.

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Once,
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_adapters::{EnvChange, LaunchPlan};
use omnis_ir::Provider;
use serde::Deserialize;
use serde_json::{Value, json};

/// Name of the profile file in the `OmniSession` state directory.
pub(crate) const PROFILES_FILE: &str = "profiles.toml";
/// Largest profile file that is read. The file holds a few dozen lines in practice.
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_LABEL_CHARACTERS: usize = 64;
const MAX_ENV_NAME_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    profiles: BTreeMap<String, RawProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    agent: String,
    label: Option<String>,
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
}

/// One validated launch profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchProfile {
    /// The name that `--in` accepts.
    pub(crate) name: String,
    /// The name that the target page shows.
    pub(crate) label: String,
    /// The built-in agent that the profile starts.
    pub(crate) agent: Provider,
    program: Option<String>,
    args: Vec<String>,
    /// Sorted by name. Values may refer to the environment of `omni` as `${NAME}`.
    env: Vec<(String, String)>,
    unset: Vec<String>,
}

impl LaunchProfile {
    /// Adds the profile to the launch plan of its agent.
    ///
    /// Profile arguments go before the agent's own launch arguments. Variables are removed first,
    /// then set, so a profile cannot lose a value that it sets. `lookup` reads the environment
    /// of `omni`, which is where `${NAME}` takes its value.
    ///
    /// # Errors
    ///
    /// Returns an error naming the profile when a value refers to a variable that is not set and
    /// has no default.
    pub(crate) fn apply(
        &self,
        mut plan: LaunchPlan,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<LaunchPlan> {
        let name = &self.name;
        let mut args = Vec::with_capacity(self.args.len());
        for (index, arg) in self.args.iter().enumerate() {
            args.push(
                expand(arg, lookup)
                    .with_context(|| format!("profile `{name}`, argument {}", index + 1))?,
            );
        }
        if let Some(program) = &self.program {
            let program =
                expand(program, lookup).with_context(|| format!("profile `{name}`, program"))?;
            if program.is_empty() {
                bail!("profile `{name}`: program is empty after expansion");
            }
            plan.program = program;
        }
        plan.args.splice(0..0, args);
        plan.env
            .extend(self.unset.iter().cloned().map(EnvChange::Remove));
        for (variable, value) in &self.env {
            let value = expand(value, lookup)
                .with_context(|| format!("profile `{name}`, variable `{variable}`"))?;
            plan.env.push(EnvChange::Set {
                name: variable.clone(),
                value,
            });
        }
        Ok(plan)
    }

    /// Whether the profile can start now. A profile that keeps the agent's command needs that
    /// agent to be installed. A profile with its own program needs that program to exist.
    pub(crate) fn is_runnable(&self, installed: &[Provider]) -> bool {
        match &self.program {
            None => installed.contains(&self.agent),
            Some(program) => expand(program, &environment)
                .is_ok_and(|program| program_exists(&program, std::env::var_os("PATH").as_deref())),
        }
    }

    /// Whether the profile runs another program instead of the agent's own command.
    pub(crate) const fn overrides_program(&self) -> bool {
        self.program.is_some()
    }

    /// Why the profile cannot start now, or `None` when it can. This also expands every value,
    /// so a missing variable shows here instead of at launch.
    pub(crate) fn problem(&self, installed: &[Provider]) -> Option<String> {
        if !self.is_runnable(installed) {
            return Some(match &self.program {
                None => format!("the agent `{}` is not installed", self.agent),
                Some(program) => {
                    format!("the program `{program}` was not found or is not executable")
                }
            });
        }
        self.check(&environment)
            .err()
            .map(|error| format!("{error:#}"))
    }

    /// Checks that every value expands, without building a launch. A run calls this before it
    /// imports or writes anything, so a missing variable never costs a rollback.
    ///
    /// # Errors
    ///
    /// Returns the error that `apply` would return.
    pub(crate) fn check(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
        self.apply(
            LaunchPlan {
                program: String::new(),
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            },
            lookup,
        )
        .map(drop)
    }

    /// What the profile changes, for `--json` listings. Argument and variable values are left
    /// out, because they may be credentials.
    pub(crate) fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "label": self.label,
            "agent": self.agent,
            "program": self.program,
            "arguments": self.args.len(),
            "sets": self.env.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "unsets": self.unset,
        })
    }

    /// One line for listings: what the profile runs and changes. Values are left out, because
    /// they may be credentials.
    pub(crate) fn summary(&self) -> String {
        let mut parts = vec![format!("agent {}", self.agent)];
        if let Some(program) = &self.program {
            parts.push(format!("program {program}"));
        }
        if !self.args.is_empty() {
            parts.push(format!("{} argument(s)", self.args.len()));
        }
        if !self.env.is_empty() {
            parts.push(format!(
                "sets {}",
                self.env
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.unset.is_empty() {
            parts.push(format!("unsets {}", self.unset.join(", ")));
        }
        parts.join("; ")
    }
}

/// Reads and validates profiles from `text`.
///
/// # Errors
///
/// Returns an error for invalid TOML, an unknown field, or any value that fails validation. The
/// message names the profile.
pub(crate) fn parse(text: &str) -> Result<Vec<LaunchProfile>> {
    let file: RawFile = toml::from_str(text).context("reading profile file")?;
    if file.profiles.len() > MAX_PROFILES {
        bail!("at most {MAX_PROFILES} profiles are allowed");
    }
    file.profiles
        .into_iter()
        .map(|(name, raw)| build(name.clone(), raw).with_context(|| format!("profile `{name}`")))
        .collect()
}

/// Reads the profile file at `path`. A missing file holds no profiles.
///
/// A file that other users can write is refused, because a profile chooses the program that
/// `omni` runs.
///
/// # Errors
///
/// Returns an error when the file is not a regular file, is too large, is writable by other
/// users, cannot be read, or does not validate.
pub(crate) fn read(path: &Path) -> Result<Vec<LaunchProfile>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading `{}`", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("`{}` is not a regular file", path.display());
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("`{}` is larger than {MAX_FILE_BYTES} bytes", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o022 != 0 {
            bail!(
                "`{}` can be written by other users; run `chmod go-w` on it",
                path.display()
            );
        }
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading `{}`", path.display()))?;
    parse(&text).with_context(|| format!("in `{}`", path.display()))
}

/// Where profiles are read from: the `OmniSession` state directory.
pub(crate) fn profiles_path() -> Result<PathBuf> {
    Ok(omnis_store::state_root()
        .context("resolving OmniSession state")?
        .join(PROFILES_FILE))
}

/// The profiles of this run, read once. The error text is kept for commands that report it.
#[cfg(not(test))]
pub(crate) fn loaded() -> &'static std::result::Result<Vec<LaunchProfile>, String> {
    static LOADED: std::sync::OnceLock<std::result::Result<Vec<LaunchProfile>, String>> =
        std::sync::OnceLock::new();
    LOADED.get_or_init(|| {
        profiles_path()
            .and_then(|path| read(&path))
            .map_err(|error| format!("{error:#}"))
    })
}

/// Unit tests pin an empty file instead of the profiles of a developer's machine.
#[cfg(test)]
pub(crate) fn loaded() -> &'static std::result::Result<Vec<LaunchProfile>, String> {
    static NONE: std::result::Result<Vec<LaunchProfile>, String> = Ok(Vec::new());
    &NONE
}

/// The value of a variable of the `omni` process, as `${NAME}` in a profile reads it.
pub(crate) fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The profiles of this run, or none when the file does not validate. A broken file prints one
/// warning, so a typo does not stop `omni` from starting. `--in` reports the same error when a
/// name cannot be found.
pub(crate) fn configured() -> &'static [LaunchProfile] {
    static WARNED: Once = Once::new();
    match loaded() {
        Ok(profiles) => profiles,
        Err(error) => {
            WARNED.call_once(|| eprintln!("warning: ignoring launch profiles: {error}"));
            &[]
        }
    }
}

/// The executable file that `program` names: a path as written, or a command found in `path`.
/// On Windows, a command also matches the extensions in `PATHEXT`, as a shell does.
fn resolve_program(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        let candidate = PathBuf::from(program);
        return crate::shim::is_executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path?)
        .flat_map(|directory| crate::shim::executable_candidates(&directory, program))
        .find(|candidate| crate::shim::is_executable(candidate))
}

/// Whether `program` names an executable file, as [`resolve_program`] finds it.
fn program_exists(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    resolve_program(program, path).is_some()
}

/// The agents and profiles that can start now, built-in agents first. `installed` lists the
/// built-in agents that were found, and a profile is listed only when it can start too.
pub(crate) fn runnable_targets(
    installed: &[Provider],
    profiles: &'static [LaunchProfile],
) -> Vec<AgentTarget> {
    installed
        .iter()
        .copied()
        .map(AgentTarget::from)
        .chain(
            profiles
                .iter()
                .filter(|profile| profile.is_runnable(installed))
                .map(|profile| AgentTarget {
                    provider: profile.agent,
                    profile: Some(profile),
                }),
        )
        .collect()
}

/// The agents and profiles that can start now, for the target page and `--in`.
pub(crate) fn runnable_agent_targets() -> Vec<AgentTarget> {
    runnable_targets(&crate::shim::runnable_target_providers(), configured())
}

/// Prints the profile file and every profile in it, with whether each can start now.
///
/// # Errors
///
/// Returns an error when the state directory cannot be resolved or the profile file does not
/// validate. The message names the file and the profile.
pub(crate) fn list(json_output: bool) -> Result<()> {
    let path = profiles_path()?;
    let profiles = loaded().as_ref().map_err(|error| anyhow!("{error}"))?;
    let installed = crate::shim::runnable_target_providers();
    let rows = profiles
        .iter()
        .map(|profile| (profile, profile.problem(&installed)))
        .collect::<Vec<_>>();
    if json_output {
        let profiles = rows
            .iter()
            .map(|(profile, problem)| {
                let mut value = profile.describe();
                value["ready"] = json!(problem.is_none());
                value["problem"] = json!(problem);
                value
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"file": path, "profiles": profiles}))?
        );
        return Ok(());
    }
    println!("Launch profiles file: {}", path.display());
    if rows.is_empty() {
        println!(
            "No launch profiles. Add `[profiles.NAME]` tables to that file; see docs/PROFILES.md."
        );
    }
    for (profile, problem) in rows {
        println!(
            "{}  {}",
            profile.name,
            crate::safe_terminal_line(&profile.label)
        );
        println!("    {}", crate::safe_terminal_line(&profile.summary()));
        match problem {
            None => println!("    ready"),
            Some(problem) => println!("    not ready: {}", crate::safe_terminal_line(&problem)),
        }
    }
    Ok(())
}

/// What `--in` names: a built-in agent, or a launch profile that starts one.
///
/// Either way the agent decides how sessions are found, imported, and forked. A profile only
/// changes how that agent is launched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AgentTarget {
    pub(crate) provider: Provider,
    pub(crate) profile: Option<&'static LaunchProfile>,
}

impl AgentTarget {
    /// The name that a person recognizes: the profile label, else the agent's display name.
    pub(crate) fn label(self, agent_name: &str) -> String {
        self.profile
            .map_or_else(|| agent_name.to_owned(), |profile| profile.label.clone())
    }

    /// Checks that the profile can expand its values now. A built-in agent always can. A run
    /// calls this before it imports or writes anything, so a missing variable costs no rollback.
    ///
    /// # Errors
    ///
    /// Returns the error of a profile value that cannot be expanded.
    pub(crate) fn check(self) -> Result<()> {
        match self.profile {
            Some(profile) => profile.check(&environment),
            None => Ok(()),
        }
    }

    /// Whether the profile runs another program instead of the agent's own command.
    pub(crate) fn overrides_program(self) -> bool {
        self.profile.is_some_and(LaunchProfile::overrides_program)
    }

    /// The file that a run executes when the profile replaces the agent's command, or `None`
    /// when the agent's own command runs. Permission modes are checked against this file,
    /// because it decides which flags a launch accepts.
    ///
    /// # Errors
    ///
    /// Returns an error when the program does not expand, does not exist, or is not executable.
    pub(crate) fn own_program(self) -> Result<Option<PathBuf>> {
        let Some(program) = self.profile.and_then(|profile| profile.program.as_deref()) else {
            return Ok(None);
        };
        let expanded =
            expand(program, &environment).with_context(|| format!("profile `{self}`, program"))?;
        resolve_program(&expanded, std::env::var_os("PATH").as_deref())
            .map(Some)
            .ok_or_else(|| anyhow!("program `{expanded}` was not found or is not executable"))
    }

    /// Adds the profile to the launch plan of the agent. A built-in agent leaves it as it is.
    ///
    /// # Errors
    ///
    /// Returns the error of a profile value that cannot be expanded.
    pub(crate) fn launch(self, plan: LaunchPlan) -> Result<LaunchPlan> {
        match self.profile {
            Some(profile) => profile.apply(plan, &environment),
            None => Ok(plan),
        }
    }
}

impl From<Provider> for AgentTarget {
    fn from(provider: Provider) -> Self {
        Self {
            provider,
            profile: None,
        }
    }
}

impl FromStr for AgentTarget {
    type Err = String;

    /// Built-in agent names win. They cannot be profile names, so there is no clash.
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        target_from(value, loaded)
    }
}

/// Resolves `value` against built-in agents first, then against the profiles that `profiles`
/// returns. A built-in name never calls `profiles`, so it never reads the profile file.
fn target_from(
    value: &str,
    profiles: impl FnOnce() -> &'static std::result::Result<Vec<LaunchProfile>, String>,
) -> std::result::Result<AgentTarget, String> {
    if let Ok(provider) = value.parse::<Provider>() {
        return Ok(provider.into());
    }
    match profiles() {
        Ok(profiles) => {
            let name = value.to_ascii_lowercase();
            if let Some(profile) = profiles.iter().find(|profile| profile.name == name) {
                return Ok(AgentTarget {
                    provider: profile.agent,
                    profile: Some(profile),
                });
            }
            let names = profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(if names.is_empty() {
                format!("unknown agent or launch profile `{value}`")
            } else {
                format!("unknown agent or launch profile `{value}`; profiles: {names}")
            })
        }
        Err(error) => Err(format!(
            "unknown agent `{value}`, and the launch profiles could not be read: {error}"
        )),
    }
}

impl fmt::Display for AgentTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.profile {
            Some(profile) => formatter.write_str(&profile.name),
            None => write!(formatter, "{}", self.provider),
        }
    }
}

fn build(name: String, raw: RawProfile) -> Result<LaunchProfile> {
    if !valid_profile_name(&name) {
        bail!(
            "the name must be 1 to {MAX_NAME_BYTES} characters of lowercase letters, digits, `.`, `_`, and `-`, and start with a letter or digit"
        );
    }
    if name.parse::<Provider>().is_ok() {
        bail!("`{name}` is the name of a built-in agent; choose another name");
    }
    let agent = raw
        .agent
        .parse::<Provider>()
        .ok()
        .filter(|agent| agent.command().is_some());
    let Some(agent) = agent else {
        let launchable = Provider::ALL
            .iter()
            .filter(|provider| provider.command().is_some())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "`agent = \"{}\"` is not an agent that profiles can start; use one of {launchable}",
            raw.agent
        );
    };
    let label = raw.label.unwrap_or_else(|| name.clone());
    if label.is_empty()
        || label.chars().count() > MAX_LABEL_CHARACTERS
        || label.chars().any(char::is_control)
    {
        bail!("`label` must be 1 to {MAX_LABEL_CHARACTERS} characters without control characters");
    }
    if let Some(program) = &raw.program {
        if program.is_empty() {
            bail!("`program` is empty");
        }
        check_value("program", program)?;
    }
    for (index, arg) in raw.args.iter().enumerate() {
        check_value(&format!("args[{}]", index + 1), arg)?;
    }
    for (variable, value) in &raw.env {
        check_variable_name(variable)?;
        check_value(&format!("env.{variable}"), value)?;
    }
    for variable in &raw.unset {
        check_variable_name(variable)?;
        if raw.env.contains_key(variable) {
            bail!("`{variable}` is both set in `env` and listed in `unset`");
        }
    }
    let mut unset = raw.unset;
    unset.sort();
    unset.dedup();
    Ok(LaunchProfile {
        name,
        label,
        agent,
        program: raw.program,
        args: raw.args,
        env: raw.env.into_iter().collect(),
        unset,
    })
}

fn valid_profile_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_ENV_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// A name that a profile may change. `OMNI_` names steer `omni` itself, and `OMNISESSION_HOME`
/// holds the state that `omni` reads, so a profile cannot reach them.
fn check_variable_name(name: &str) -> Result<()> {
    if !valid_variable_name(name) {
        bail!("`{name}` is not a valid variable name");
    }
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("OMNI_") || upper == "OMNISESSION_HOME" {
        bail!(
            "`{name}` is reserved: profiles cannot change `OMNI_` variables or `OMNISESSION_HOME`"
        );
    }
    Ok(())
}

/// A value that is valid text for a process and has well-formed `${...}` references.
fn check_value(field: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("`{field}` contains a NUL character");
    }
    expand(value, &|_| Some("x".to_owned())).with_context(|| format!("`{field}`"))?;
    Ok(())
}

/// Expands `${NAME}` and `${NAME:-default}` in `value`, and `$$` to a literal `$`.
///
/// `${NAME}` needs `NAME` to be set. `${NAME:-default}` uses the default when `NAME` is unset or
/// empty, as a shell does. A default ends at the first `}`, so it cannot hold one. Any other `$`
/// stays as it is.
fn expand(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('$') {
        expanded.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            expanded.push('$');
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('{') {
            let end = tail
                .find('}')
                .ok_or_else(|| anyhow!("`${{` has no closing `}}`"))?;
            let (name, default) = match tail[..end].split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (&tail[..end], None),
            };
            if !valid_variable_name(name) {
                bail!("`{name}` is not a valid variable name");
            }
            match (lookup(name), default) {
                (Some(found), _) if !found.is_empty() => expanded.push_str(&found),
                (Some(found), None) => expanded.push_str(&found),
                (_, Some(default)) => expanded.push_str(default),
                (_, None) => bail!(
                    "variable `{name}` is not set; write `${{{name}:-default}}` to give it a default"
                ),
            }
            rest = &tail[end + 1..];
        } else {
            expanded.push('$');
            rest = after;
        }
    }
    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fmt::Write as _};

    use omnis_adapters::{EnvChange, LaunchPlan};
    use omnis_ir::Provider;

    use super::{AgentTarget, LaunchProfile, expand, parse, read};

    fn lookup<'a>(variables: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let variables = variables
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        move |name| variables.get(name).cloned()
    }

    fn plan() -> LaunchPlan {
        LaunchPlan {
            program: "claude".to_owned(),
            args: vec!["--resume".to_owned(), "session".to_owned()],
            cwd: None,
            env: Vec::new(),
        }
    }

    fn one(text: &str) -> LaunchProfile {
        let mut profiles = parse(text).expect("valid profile file");
        assert_eq!(profiles.len(), 1);
        profiles.remove(0)
    }

    fn error(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("invalid profile file"))
    }

    const GATEWAY_CLAUDE: &str = r#"
[profiles.claude-local]
label = "Claude Code (local gateway)"
agent = "claude"
args = ["--settings", "${HOME}/gateway-settings.json"]
unset = ["ANTHROPIC_API_KEY"]

[profiles.claude-local.env]
ANTHROPIC_BASE_URL = "${GATEWAY_URL:-http://127.0.0.1:9999}"
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN:-local-token}"
"#;

    #[test]
    fn profile_file_without_profiles_is_valid() {
        assert!(parse("").expect("empty file").is_empty());
        assert!(parse("[profiles]\n").expect("empty table").is_empty());
    }

    #[test]
    fn profile_starts_its_agent_with_arguments_and_environment_changes() {
        let profile = one(GATEWAY_CLAUDE);
        assert_eq!(profile.name, "claude-local");
        assert_eq!(profile.label, "Claude Code (local gateway)");
        assert_eq!(profile.agent, Provider::Claude);

        let plan = profile
            .apply(plan(), &lookup(&[("HOME", "/home/user")]))
            .expect("apply profile");
        assert_eq!(plan.program, "claude");
        assert_eq!(
            plan.args,
            [
                "--settings",
                "/home/user/gateway-settings.json",
                "--resume",
                "session"
            ]
        );
        assert_eq!(
            plan.env,
            [
                EnvChange::Remove("ANTHROPIC_API_KEY".to_owned()),
                EnvChange::Set {
                    name: "ANTHROPIC_AUTH_TOKEN".to_owned(),
                    value: "local-token".to_owned()
                },
                EnvChange::Set {
                    name: "ANTHROPIC_BASE_URL".to_owned(),
                    value: "http://127.0.0.1:9999".to_owned()
                },
            ]
        );
    }

    #[test]
    fn environment_of_omni_overrides_the_defaults_of_a_profile() {
        let plan = one(GATEWAY_CLAUDE)
            .apply(
                plan(),
                &lookup(&[
                    ("HOME", "/home/user"),
                    ("GATEWAY_URL", "http://10.0.0.5:1"),
                    ("GATEWAY_TOKEN", ""),
                ]),
            )
            .expect("apply profile");
        let values = plan
            .env
            .iter()
            .filter_map(|change| match change {
                EnvChange::Set { name, value } => Some((name.as_str(), value.as_str())),
                EnvChange::Remove(_) => None,
            })
            .collect::<Vec<_>>();
        // An empty variable counts as unset for `:-`, as in a shell.
        assert_eq!(
            values,
            [
                ("ANTHROPIC_AUTH_TOKEN", "local-token"),
                ("ANTHROPIC_BASE_URL", "http://10.0.0.5:1"),
            ]
        );
    }

    #[test]
    fn program_replaces_the_agent_command_and_keeps_its_launch_arguments() {
        let profile = one(r#"
[profiles.wrapped]
agent = "codex"
program = "${HOME}/bin/codex-wrapper"
"#);
        assert_eq!(profile.label, "wrapped");
        let mut plan = plan();
        plan.program = "codex".to_owned();
        let plan = profile
            .apply(plan, &lookup(&[("HOME", "/home/user")]))
            .expect("apply profile");
        assert_eq!(plan.program, "/home/user/bin/codex-wrapper");
        assert_eq!(plan.args, ["--resume", "session"]);
        assert!(plan.env.is_empty());
    }

    #[test]
    fn missing_variable_without_a_default_names_the_profile_and_field() {
        let profile = one(r#"
[profiles.needs-token]
agent = "claude"
[profiles.needs-token.env]
ANTHROPIC_AUTH_TOKEN = "${GATEWAY_TOKEN}"
"#);
        let message = format!(
            "{:#}",
            profile
                .apply(plan(), &lookup(&[]))
                .expect_err("unset variable")
        );
        assert!(message.contains("profile `needs-token`"), "{message}");
        assert!(message.contains("ANTHROPIC_AUTH_TOKEN"), "{message}");
        assert!(message.contains("GATEWAY_TOKEN"), "{message}");
        assert!(message.contains(":-default"), "{message}");
    }

    #[test]
    fn expansion_handles_defaults_escapes_and_literal_dollars() {
        let variables = lookup(&[("SET", "value"), ("EMPTY", "")]);
        let cases = [
            ("plain", "plain"),
            ("${SET}", "value"),
            ("${SET:-other}", "value"),
            ("${EMPTY}", ""),
            ("${EMPTY:-other}", "other"),
            ("${MISSING:-}", ""),
            ("${MISSING:-a b/c}", "a b/c"),
            ("a${SET}b${SET}c", "avaluebvaluec"),
            ("$$", "$"),
            ("$${SET}", "${SET}"),
            ("cost: $5 and $", "cost: $5 and $"),
            ("-c=\"x=${SET}/v1\"", "-c=\"x=value/v1\""),
        ];
        for (input, expected) in cases {
            assert_eq!(expand(input, &variables).expect(input), expected, "{input}");
        }
        for malformed in ["${", "${SET", "${}", "${1BAD}", "${A-B}", "${SET:-x"] {
            assert!(expand(malformed, &variables).is_err(), "{malformed}");
        }
    }

    #[test]
    fn built_in_agents_are_targets_without_reading_any_file() {
        for (name, provider) in [
            ("claude", Provider::Claude),
            ("Codex", Provider::Codex),
            ("agy", Provider::Antigravity),
            ("oh-my-pi", Provider::OhMyPi),
        ] {
            let target = super::target_from(name, || {
                panic!("a built-in agent name must not read the profiles")
            })
            .expect(name);
            assert_eq!(
                (target.provider, target.profile),
                (provider, None),
                "{name}"
            );
            assert_eq!(target, AgentTarget::from(provider));
        }
        assert_eq!(AgentTarget::from(Provider::Grok).to_string(), "grok");
    }

    /// Profiles that live as long as the process, like the ones read from the state directory.
    fn leaked(text: &str) -> &'static std::result::Result<Vec<LaunchProfile>, String> {
        Box::leak(Box::new(Ok(parse(text).expect("valid profile file"))))
    }

    #[test]
    fn profile_names_resolve_to_their_agent_after_built_in_names() {
        let profiles = leaked(GATEWAY_CLAUDE);
        let target = super::target_from("claude-local", || profiles).expect("profile name");
        assert_eq!(target.provider, Provider::Claude);
        assert_eq!(
            target.profile.map(|profile| profile.name.as_str()),
            Some("claude-local")
        );
        assert_eq!(target.to_string(), "claude-local");
        // Profile names are lowercase, and a typed name is matched without regard to case.
        assert_eq!(super::target_from("Claude-Local", || profiles), Ok(target));
        // A built-in name never reaches the profiles.
        assert_eq!(
            super::target_from("claude", || profiles),
            Ok(AgentTarget::from(Provider::Claude))
        );
    }

    #[test]
    fn unknown_names_list_the_profiles_that_exist() {
        let message =
            super::target_from("nothing", || leaked(GATEWAY_CLAUDE)).expect_err("unknown");
        assert!(message.contains("`nothing`"), "{message}");
        assert!(message.contains("profiles: claude-local"), "{message}");
        let message = super::target_from("nothing", || leaked("")).expect_err("unknown");
        assert_eq!(message, "unknown agent or launch profile `nothing`");
    }

    #[test]
    fn unreadable_profile_file_is_reported_when_a_name_is_not_built_in() {
        let broken: &'static std::result::Result<Vec<LaunchProfile>, String> =
            Box::leak(Box::new(Err("line 3: invalid".to_owned())));
        let message = super::target_from("claude-local", || broken).expect_err("unreadable");
        assert!(message.contains("could not be read"), "{message}");
        assert!(message.contains("line 3: invalid"), "{message}");
        // Built-in agents still work when the file is broken.
        assert!(super::target_from("codex", || broken).is_ok());
    }

    #[test]
    fn runnable_targets_list_built_in_agents_first_and_hide_profiles_without_their_agent() {
        let profiles: &'static [LaunchProfile] = Box::leak(
            parse(
                r#"
[profiles.claude-local]
agent = "claude"
[profiles.codex-local]
agent = "codex"
[profiles.wrapped]
agent = "claude"
program = "/nonexistent/wrapper"
"#,
            )
            .expect("valid profiles")
            .into_boxed_slice(),
        );
        let names = |installed: &[Provider]| {
            super::runnable_targets(installed, profiles)
                .into_iter()
                .map(|target| target.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&[Provider::Claude]), ["claude", "claude-local"]);
        assert_eq!(
            names(&[Provider::Codex, Provider::Claude]),
            ["codex", "claude", "claude-local", "codex-local"]
        );
        // A profile whose program does not exist is hidden even when its agent is installed.
        assert!(!names(&[Provider::Claude]).contains(&"wrapped".to_owned()));
        assert!(names(&[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn profile_with_its_own_program_is_runnable_when_the_program_is_executable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let program = directory.path().join("wrapper");
        std::fs::write(&program, "#!/bin/sh\n").expect("write wrapper");
        let profile = one(&format!(
            "[profiles.wrapped]\nagent = \"claude\"\nprogram = \"{}\"\n",
            program.display()
        ));
        assert!(
            !profile.is_runnable(&[Provider::Claude]),
            "not executable yet"
        );
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make wrapper executable");
        // The agent itself does not have to be installed when the profile brings its own program.
        assert!(profile.is_runnable(&[]));
        assert!(super::program_exists(
            "sh",
            std::env::var_os("PATH").as_deref()
        ));
        assert!(!super::program_exists(
            "no-such-program-anywhere",
            std::env::var_os("PATH").as_deref()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn own_program_is_the_executable_that_a_profile_names() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let program = directory.path().join("wrapper");
        std::fs::write(&program, "#!/bin/sh\n").expect("write wrapper");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make wrapper executable");
        let profiles = leaked(&format!(
            "[profiles.wrapped]\nagent = \"claude\"\nprogram = \"{}\"\n\
             [profiles.missing]\nagent = \"claude\"\nprogram = \"{}/none\"\n\
             [profiles.plain]\nagent = \"claude\"\n",
            program.display(),
            directory.path().display()
        ));
        let target = |name: &str| super::target_from(name, || profiles).expect(name);
        assert_eq!(
            target("wrapped").own_program().expect("program"),
            Some(program)
        );
        assert!(target("missing").own_program().is_err());
        // Without a program of its own, the agent's command runs, so there is nothing to check.
        assert_eq!(target("plain").own_program().expect("no program"), None);
        assert_eq!(
            AgentTarget::from(Provider::Claude)
                .own_program()
                .expect("built in"),
            None
        );
        assert!(super::resolve_program("sh", std::env::var_os("PATH").as_deref()).is_some());
    }

    #[test]
    fn target_label_prefers_the_profile_label() {
        let profile = leaked(GATEWAY_CLAUDE);
        let target = super::target_from("claude-local", || profile).expect("profile name");
        assert_eq!(target.label("Claude"), "Claude Code (local gateway)");
        assert_eq!(
            AgentTarget::from(Provider::Claude).label("Claude"),
            "Claude"
        );
    }

    #[test]
    fn target_launch_changes_only_profile_launches() {
        let plain = AgentTarget::from(Provider::Claude)
            .launch(plan())
            .expect("built-in launch");
        assert_eq!(plain, plan());

        // The variable has a default, so the result does not depend on the test environment.
        let profiles = leaked(
            r#"
[profiles.defaults-only]
agent = "claude"
args = ["--model", "${OMNI_TEST_NO_SUCH_VARIABLE:-fixed}"]
"#,
        );
        let target = super::target_from("defaults-only", || profiles).expect("profile name");
        let launched = target.launch(plan()).expect("profile launch");
        assert_eq!(launched.args, ["--model", "fixed", "--resume", "session"]);
    }

    #[test]
    fn summary_lists_names_and_never_values() {
        let summary = one(GATEWAY_CLAUDE).summary();
        assert_eq!(
            summary,
            "agent claude; 2 argument(s); sets ANTHROPIC_AUTH_TOKEN, ANTHROPIC_BASE_URL; unsets ANTHROPIC_API_KEY"
        );
        assert!(!summary.contains("local-token"));
    }

    #[test]
    fn invalid_profiles_are_refused_with_the_profile_and_the_reason() {
        let cases = [
            ("[profiles.Bad]\nagent = \"claude\"", "name must be"),
            ("[profiles.\"\"]\nagent = \"claude\"", "name must be"),
            ("[profiles.claude]\nagent = \"claude\"", "built-in agent"),
            (
                "[profiles.codex-cli]\nagent = \"claude\"\n[profiles.cursor]\nagent = \"codex\"",
                "built-in agent",
            ),
            (
                "[profiles.a]\nagent = \"nothing\"",
                "not an agent that profiles can start",
            ),
            (
                "[profiles.a]\nagent = \"imported\"",
                "not an agent that profiles can start",
            ),
            ("[profiles.a]\nagent = \"claude\"\nsurprise = 1", "surprise"),
            ("[profiles.a]\nlabel = \"x\"", "agent"),
            ("[profiles.a]\nagent = \"claude\"\nlabel = \"\"", "label"),
            (
                "[profiles.a]\nagent = \"claude\"\nlabel = \"bad\\u001b[31m\"",
                "label",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nprogram = \"\"",
                "program",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nargs = [\"x\\u0000y\"]",
                "NUL",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nargs = [\"${\"]",
                "args[1]",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\n[profiles.a.env]\n\"BAD NAME\" = \"x\"",
                "variable name",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\n[profiles.a.env]\nOMNI_MODE = \"yolo\"",
                "reserved",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nunset = [\"omni_bypass\"]",
                "reserved",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nunset = [\"omnisession_home\"]",
                "reserved",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\n[profiles.a.env]\nOMNISESSION_HOME = \"/tmp/state\"",
                "reserved",
            ),
            (
                "[profiles.a]\nagent = \"claude\"\nunset = [\"X\"]\n[profiles.a.env]\nX = \"1\"",
                "both set",
            ),
        ];
        for (text, expected) in cases {
            let message = error(text);
            assert!(message.contains(expected), "{text}\n  -> {message}");
        }
        let message = error("[profiles.a]\nagent = \"nothing\"");
        assert!(message.contains("profile `a`"), "{message}");
        assert!(message.contains("claude, codex"), "{message}");
    }

    #[test]
    fn describe_and_check_never_expose_values() {
        let profile = one(GATEWAY_CLAUDE);
        let described = profile.describe();
        assert_eq!(described["agent"], "claude");
        assert_eq!(described["arguments"], 2);
        assert_eq!(
            described["sets"],
            serde_json::json!(["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"])
        );
        assert!(!described.to_string().contains("local-token"));
        assert!(profile.check(&lookup(&[("HOME", "/h")])).is_ok());
        assert!(profile.check(&lookup(&[])).is_err());
        assert!(!profile.overrides_program());
    }

    #[test]
    fn invalid_toml_and_oversized_files_are_refused() {
        assert!(parse("[profiles").is_err());
        let mut many = String::new();
        for index in 0..65 {
            writeln!(many, "[profiles.p{index}]\nagent = \"claude\"").expect("write to a string");
        }
        assert!(error(&many).contains("at most 64"));
    }

    mod file {
        use std::fs;

        use tempfile::tempdir;

        use super::{GATEWAY_CLAUDE, read};

        #[test]
        fn missing_file_holds_no_profiles() {
            let directory = tempdir().expect("temporary directory");
            assert!(
                read(&directory.path().join("profiles.toml"))
                    .expect("missing file")
                    .is_empty()
            );
        }

        #[test]
        fn file_is_read_and_errors_name_the_file() {
            let directory = tempdir().expect("temporary directory");
            let path = directory.path().join("profiles.toml");
            fs::write(&path, GATEWAY_CLAUDE).expect("write profiles");
            assert_eq!(read(&path).expect("valid file").len(), 1);

            fs::write(&path, "[profiles.a]\nagent = \"nothing\"").expect("write bad profiles");
            let message = format!("{:#}", read(&path).expect_err("invalid file"));
            assert!(message.contains("profiles.toml"), "{message}");
            assert!(message.contains("profile `a`"), "{message}");
        }

        #[test]
        fn directory_and_oversized_file_are_refused() {
            let directory = tempdir().expect("temporary directory");
            assert!(read(directory.path()).is_err());
            let path = directory.path().join("profiles.toml");
            fs::write(&path, " ".repeat(300 * 1024)).expect("write large file");
            assert!(format!("{:#}", read(&path).expect_err("large file")).contains("larger than"));
        }

        #[cfg(unix)]
        #[test]
        fn file_that_other_users_can_write_is_refused() {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempdir().expect("temporary directory");
            let path = directory.path().join("profiles.toml");
            fs::write(&path, GATEWAY_CLAUDE).expect("write profiles");
            for mode in [0o666, 0o620, 0o602] {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set mode");
                let message = format!("{:#}", read(&path).expect_err("writable file"));
                assert!(message.contains("chmod go-w"), "{mode:o}: {message}");
            }
            for mode in [0o600, 0o644] {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set mode");
                assert!(read(&path).is_ok(), "{mode:o}");
            }
        }

        #[cfg(unix)]
        #[test]
        fn symlinked_file_is_followed() {
            let directory = tempdir().expect("temporary directory");
            let real = directory.path().join("real.toml");
            fs::write(&real, GATEWAY_CLAUDE).expect("write profiles");
            let link = directory.path().join("profiles.toml");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            assert_eq!(read(&link).expect("symlinked file").len(), 1);
        }
    }
}
