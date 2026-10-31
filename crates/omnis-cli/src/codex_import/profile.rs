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
