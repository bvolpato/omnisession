use std::{collections::BTreeMap, fs, io::Read, path::Path, process::Command};

use anyhow::{Context, Result, anyhow, bail};
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
        let mut profile = None;
        let mut provider = None;
        let mut names = BTreeMap::new();
        let mut args = args.iter().map(String::as_str);
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
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

pub(crate) fn launch_selects_session(
    adapter: &dyn omnis_adapters::ProviderAdapter,
    session: &omnis_ir::SessionRef,
    profile: Option<&crate::launch_profile::LaunchProfile>,
    directory: &Path,
) -> Result<bool> {
    use omnis_adapters::{CodexLaunchOverrides, EnvChange, LaunchPlan};

    let mut overrides = CodexLaunchOverrides::default();
    let Some(profile) = profile.filter(|_| session.provider == omnis_ir::Provider::Codex) else {
        return adapter.native_selects_session_at(session, directory, &overrides);
    };
    let plan = profile.apply(
        LaunchPlan {
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        },
        &crate::launch_profile::environment,
    )?;
    let mut launch_dir = directory.to_owned();
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
            let (key, value) = assignment
                .split_once('=')
                .context("Codex -c requires key=value")?;
            if key.trim() == "sqlite_home" {
                overrides.sqlite_home = Some(config_string(value)?.into());
            }
        }
    }
    overrides.environment = plan.env.into_iter().rev().find(|change| match change {
        EnvChange::Set { name, .. } | EnvChange::Remove(name) => name == "CODEX_SQLITE_HOME",
    });
    adapter.native_selects_session_at(session, &launch_dir, &overrides)
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
        Ok(_) => bail!("Codex configuration override must be a string"),
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
