use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_adapters::{CodexLaunchOverrides, EnvChange, LaunchPlan};
use serde::Deserialize;

/// Codex accepts `--profile` on `resume` but not on `app-server`. The importer only needs the
/// provider identity, not the gateway credentials or the interactive permission flags.
#[derive(Default)]
pub(super) struct ImportProfile {
    provider: Option<String>,
    declaration: Option<String>,
}

#[derive(Default, Deserialize)]
struct CodexConfig {
    model_provider: Option<String>,
    #[serde(default)]
    model_providers: BTreeMap<String, ProviderDefinition>,
}

#[derive(Deserialize)]
struct ProviderDefinition {
    name: Option<String>,
}

impl ImportProfile {
    pub(super) fn resolve(args: &[String], home: &Path) -> Result<Self> {
        require_continuation_args(args, true)?;
        let mut profile = None;
        let mut provider = None;
        let mut names = BTreeMap::new();
        let mut args = args.iter().map(String::as_str);
        while let Some(arg) = args.next() {
            if let Some(value) = option_value(arg, "--profile", "-p", &mut args)? {
                if profile.replace(value).is_some() {
                    bail!("Codex launch specifies more than one --profile");
                }
            } else if let Some(value) = option_value(arg, "--config", "-c", &mut args)? {
                let (key, value) = value
                    .split_once('=')
                    .context("Codex -c requires key=value")?;
                let key = key.trim();
                if key == "model_provider" {
                    provider = Some(config_string(value)?);
                } else if let Some(id) = key
                    .strip_prefix("model_providers.")
                    .and_then(|k| k.strip_suffix(".name"))
                {
                    names.insert(id, config_string(value)?);
                }
            } else if arg == "--oss" {
                bail!("native Codex import cannot determine the --oss provider");
            }
        }
        let mut config = profile
            .map(|name| read_profile(home, name))
            .transpose()?
            .unwrap_or_default();
        let Some(provider) = provider.or(config.model_provider) else {
            return Ok(Self::default());
        };
        // Register providers declared only in the profile. The importer needs no connection to
        // the model service. Do not copy URLs, headers, tokens, or auth commands into its argv.
        let declaration = names
            .remove(provider.as_str())
            .or_else(|| config.model_providers.remove(&provider)?.name);
        Ok(Self {
            provider: Some(provider),
            declaration,
        })
    }

    pub(super) fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub(super) fn apply(&self, command: &mut Command) {
        if let Some(id) = &self.provider {
            let id = serde_json::Value::from(id.as_str());
            command.arg("-c").arg(format!("model_provider={id}"));
            if let Some(name) = &self.declaration {
                let name = serde_json::Value::from(name.as_str());
                command
                    .arg("-c")
                    .arg(format!("model_providers={{ {id} = {{ name = {name} }} }}"));
            }
        }
    }

    pub(super) fn verify(&self, thread: &serde_json::Value) -> Result<()> {
        if self
            .provider
            .as_deref()
            .is_some_and(|id| thread["modelProvider"].as_str() != Some(id))
        {
            bail!("imported Codex thread does not use the selected model provider");
        }
        Ok(())
    }
}

pub(super) fn require_continuation_args(args: &[String], built_in: bool) -> Result<()> {
    let mut remote_auth = false;
    for arg in args.iter().map(String::as_str) {
        if built_in && arg == "--" {
            bail!(
                "The Codex launch profile places `--` before the Codex continuation command. Remove `--` to let Codex parse `resume` or `fork`"
            );
        }
        match arg.split_once('=').map_or(arg, |(name, _)| name) {
            "--remote" => bail!(
                "OmniSession cannot verify local Codex history on the app server selected by `--remote`"
            ),
            "--remote-auth-token-env" => remote_auth = true,
            _ => {}
        }
    }
    if remote_auth {
        bail!("Codex requires `--remote` with `--remote-auth-token-env`");
    }
    Ok(())
}

pub(crate) fn pin_local_server(
    adapter: &dyn omnis_adapters::ProviderAdapter,
    session: Option<&omnis_ir::SessionRef>,
    plan: &mut omnis_adapters::LaunchPlan,
) -> Result<bool> {
    let Some(home) = selected_sqlite_home(adapter, session, plan)? else {
        return Ok(false);
    };
    let index = match session {
        Some(session) => plan
            .args
            .windows(2)
            .rposition(|args| {
                matches!(args[0].as_str(), "resume" | "fork") && args[1] == session.id
            })
            .context("Codex launch does not select the requested session ID")?,
        None => plan
            .args
            .len()
            .checked_sub(1)
            .context("Codex launch has no handoff prompt")?,
    };
    // A running daemon can use another store. A sqlite_home override selects the embedded server.
    plan.args.splice(
        index..index,
        ["-c".to_owned(), sqlite_home_argument(&home)?],
    );
    Ok(true)
}

pub(crate) fn profile_working_directory(
    target: crate::launch_profile::AgentTarget,
    process_directory: &Path,
) -> Result<PathBuf> {
    let Some(profile) = target
        .profile
        .filter(|_| target.provider == omnis_ir::Provider::Codex)
    else {
        return Ok(process_directory.to_owned());
    };
    if !target.uses_provider_program()? {
        return Ok(process_directory.to_owned());
    }
    let plan = profile.apply(
        omnis_adapters::LaunchPlan {
            program: "codex".to_owned(),
            args: Vec::new(),
            cwd: Some(process_directory.to_owned()),
            env: Vec::new(),
        },
        &crate::launch_profile::environment,
    )?;
    launch_directory(&plan)
}

pub(super) fn sqlite_home_argument(home: &Path) -> Result<String> {
    let home = home
        .to_str()
        .context("Codex cannot represent the verified SQLite home in its launch arguments")?;
    let home = serde_json::to_string(home)?.replace('\u{7f}', "\\u007f");
    Ok(format!("sqlite_home={home}"))
}

pub(crate) fn selected_sqlite_home(
    adapter: &dyn omnis_adapters::ProviderAdapter,
    session: Option<&omnis_ir::SessionRef>,
    plan: &LaunchPlan,
) -> Result<Option<PathBuf>> {
    require_continuation_args(&plan.args, false)?;
    let (launch_dir, overrides) = launch_options(plan)?;
    let Some(overrides) = overrides else {
        return Ok(None);
    };
    let expands_tilde = std::cell::OnceCell::new();
    let expand_env_tilde = || {
        expands_tilde
            .get_or_init(|| codex_expands_env_tilde(plan))
            .context("OmniSession cannot verify how this Codex version resolves a leading tilde in CODEX_SQLITE_HOME. Set CODEX_SQLITE_HOME to an absolute path")
    };
    adapter.codex_continuation_sqlite_home(session, &launch_dir, &overrides, &expand_env_tilde)
}

fn codex_expands_env_tilde(plan: &LaunchPlan) -> Option<bool> {
    let probe = LaunchPlan {
        args: Vec::new(),
        ..plan.clone()
    };
    let command = crate::prepare_launch_command(&probe).ok()?;
    let version = crate::version_gate::command_version(
        command,
        Path::new(&plan.program),
        omnis_ir::Provider::Codex,
    )
    .ok()?;
    omnis_adapters::CodexAdapter::expands_sqlite_env_tilde(&version)
}

pub(crate) fn launch_directory(plan: &LaunchPlan) -> Result<PathBuf> {
    launch_options(plan).map(|(directory, _)| directory)
}

fn launch_options(plan: &LaunchPlan) -> Result<(PathBuf, Option<CodexLaunchOverrides>)> {
    let directory = plan
        .cwd
        .as_deref()
        .context("Codex launch has no working directory")?;
    let mut overrides = CodexLaunchOverrides::default();
    let mut launch_dir = directory.to_owned();
    let mut unverified_project_config = false;
    let mut args = plan.args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(path) = option_value(arg, "--cd", "-C", &mut args)? {
            launch_dir = directory.join(path);
        } else if let Some(name) = option_value(arg, "--profile", "-p", &mut args)? {
            if !plain_name(name) {
                bail!("Codex --profile requires a plain profile name");
            }
            overrides.profile = Some(name.to_owned());
        } else if let Some(assignment) = option_value(arg, "--config", "-c", &mut args)? {
            unverified_project_config |= !apply_state_override(assignment, &mut overrides)?;
        }
    }
    if unverified_project_config && overrides.sqlite_home.is_none() {
        return Ok((launch_dir, None));
    }
    overrides.environment = plan
        .env
        .iter()
        .rev()
        .find(|change| match change {
            EnvChange::Set { name, .. } | EnvChange::Remove(name) => {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case("CODEX_SQLITE_HOME")
                } else {
                    name == "CODEX_SQLITE_HOME"
                }
            }
        })
        .cloned();
    Ok((launch_dir, Some(overrides)))
}

fn apply_state_override(
    assignment: &str,
    overrides: &mut omnis_adapters::CodexLaunchOverrides,
) -> Result<bool> {
    let (key, value) = assignment
        .split_once('=')
        .context("Codex -c requires key=value")?;
    let key = key.trim();
    match key {
        "sqlite_home" => {
            let path = config_string(value).context("Codex sqlite_home must be a string")?;
            overrides.sqlite_home = Some(path.into());
        }
        "project_root_markers" => {
            overrides.project_root_markers = Some(
                value
                    .trim()
                    .parse::<toml::Value>()
                    .ok()
                    .and_then(|value| value.try_into().ok())
                    .context("Codex project_root_markers must be an array of strings")?,
            );
        }
        // Codex splits CLI keys on literal dots. Project trust tables can change the selected database.
        _ if matches!(
            key.split('.').next(),
            Some("projects" | "project_root_markers" | "sqlite_home")
        ) =>
        {
            return Ok(false);
        }
        _ => {}
    }
    Ok(true)
}

/// Whether plain `codex`, with no launch profile, can open a session that uses `provider`.
///
/// A configuration that cannot be read answers yes. Codex then reports the problem itself, and
/// `omni` does not change the launch on a guess.
pub(super) fn plain_codex_supplies(home: &Path, provider: &str) -> bool {
    const MAX_BYTES: u64 = 1024 * 1024;
    // Codex defines these providers without any configuration.
    if matches!(provider, "openai" | "ollama" | "lmstudio") {
        return true;
    }
    let path = home.join("config.toml");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return true;
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return true;
    };
    // Only provider names are read. The rest of the file stays out of memory and out of errors.
    toml::from_str::<CodexConfig>(&text).map_or(true, |config| {
        config.model_provider.as_deref() == Some(provider)
            || config.model_providers.contains_key(provider)
    })
}

fn config_string(value: &str) -> Result<String> {
    let value = value.trim();
    match value.parse::<toml::Value>() {
        Ok(toml::Value::String(s)) => Ok(s),
        Err(_) => Ok(value.to_owned()),
        Ok(_) => bail!("Codex model provider override must be a string"),
    }
}

fn option_value<'a>(
    arg: &'a str,
    long: &str,
    short: &str,
    args: &mut impl Iterator<Item = &'a str>,
) -> Result<Option<&'a str>> {
    if arg == long || arg == short {
        return args
            .next()
            .map(Some)
            .with_context(|| format!("Codex {long} requires a value"));
    }
    Ok(arg
        .strip_prefix(long)
        .and_then(|s| s.strip_prefix('='))
        .or_else(|| {
            arg.strip_prefix(short)
                .map(|s| s.strip_prefix('=').unwrap_or(s))
        }))
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn read_profile(home: &Path, name: &str) -> Result<CodexConfig> {
    const MAX_BYTES: u64 = 1024 * 1024;
    if !plain_name(name) {
        bail!("Codex --profile requires a plain profile name");
    }
    let path = home.join(format!("{name}.config.toml"));
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        open(
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(fs::File::from)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
    };
    let file = file.with_context(|| format!("opening Codex profile `{name}`"))?;
    if !file.metadata()?.is_file() {
        bail!("Codex profile `{name}` must be a regular file");
    }
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_BYTES {
        bail!("Codex profile `{name}` is too large");
    }
    // TOML diagnostics quote the input, which can contain credentials.
    toml::from_str(&text).map_err(|_| anyhow!("cannot parse Codex profile `{name}`"))
}
