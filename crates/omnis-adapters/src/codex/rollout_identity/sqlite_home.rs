mod project;

use std::{
    collections::HashMap,
    env, fs,
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

use crate::{CodexLaunchOverrides, EnvChange};

use super::super::rollout_cache::RolloutStamp;

#[derive(Debug, Default)]
pub(super) struct SqliteHome {
    environment: Option<PathBuf>,
    config: Mutex<HashMap<PathBuf, CachedConfig>>,
}

#[derive(Debug)]
struct CachedConfig {
    stamp: RolloutStamp,
    value: Arc<Config>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Config {
    sqlite_home: Option<PathBuf>,
    #[serde(default)]
    projects: HashMap<String, Project>,
    project_root_markers: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct Project {
    trust_level: Option<Trust>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Trust {
    Trusted,
    Untrusted,
}

impl SqliteHome {
    pub(super) fn from_env() -> Self {
        let environment = env::var("CODEX_SQLITE_HOME")
            .ok()
            .as_deref()
            .and_then(environment_path)
            .map(Path::to_owned);
        Self {
            environment,
            ..Self::default()
        }
    }

    pub(super) fn resolve(&self, home: &Path) -> Result<PathBuf> {
        self.resolve_in(home, &env::current_dir()?, &CodexLaunchOverrides::default())
    }

    pub(super) fn resolve_in(
        &self,
        home: &Path,
        cwd: &Path,
        overrides: &CodexLaunchOverrides,
    ) -> Result<PathBuf> {
        if let Some(path) = &overrides.sqlite_home {
            return absolute_in(path, cwd);
        }
        if let Some(user) = self.user_settings(home, overrides.profile.as_deref())? {
            if let Some(project) = self.project_home(home, cwd, &user)? {
                return Ok(project);
            }
            if let Some(path) = &user.sqlite_home {
                return absolute_in(path, home);
            }
        }
        let environment = match &overrides.environment {
            Some(EnvChange::Set { value, .. }) => environment_path(value),
            Some(EnvChange::Remove(_)) => None,
            None => self.environment.as_deref(),
        };
        if let Some(path) = environment {
            return absolute_in(path, cwd);
        }
        Ok(home.to_owned())
    }

    fn user_settings(&self, home: &Path, profile: Option<&str>) -> Result<Option<Arc<Config>>> {
        let base = self.configured(home.join("config.toml"))?;
        let Some(name) = profile else {
            return Ok(base);
        };
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "Codex --profile requires a plain profile name"
        );
        let Some(profile) = self.configured(home.join(format!("{name}.config.toml")))? else {
            return Ok(base);
        };
        let Some(base) = base else {
            return Ok(Some(profile));
        };
        let mut user = Arc::unwrap_or_clone(base);
        if profile.sqlite_home.is_some() {
            user.sqlite_home.clone_from(&profile.sqlite_home);
        }
        if profile.project_root_markers.is_some() {
            user.project_root_markers
                .clone_from(&profile.project_root_markers);
        }
        for (name, project) in &profile.projects {
            if project.trust_level.is_some() {
                user.projects.insert(name.clone(), *project);
            }
        }
        Ok(Some(Arc::new(user)))
    }

    fn configured(&self, path: PathBuf) -> Result<Option<Arc<Config>>> {
        const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading Codex config.toml metadata"),
        };
        let path = if metadata.is_symlink() {
            let target = path.canonicalize().context("resolving Codex config.toml")?;
            let protected = target.components().any(|part| {
                let name = part.as_os_str().to_string_lossy();
                [
                    "auth.json",
                    "oauth_creds.json",
                    ".credentials.json",
                    "credentials.json",
                    ".credentials",
                    ".creds",
                    ".ssh",
                    ".gnupg",
                ]
                .iter()
                .any(|protected| name.eq_ignore_ascii_case(protected))
            });
            ensure!(
                !protected,
                "Codex config.toml points to a credential location"
            );
            target
        } else {
            path
        };
        let metadata = fs::metadata(&path).context("checking Codex config.toml")?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_CONFIG_BYTES,
            "Codex config.toml must be a regular file no larger than 4 MiB"
        );
        let stamp = cfg!(unix).then(|| RolloutStamp::new(&metadata)).flatten();
        if let Some(cached) = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&path)
            && Some(cached.stamp) == stamp
        {
            return Ok(Some(Arc::clone(&cached.value)));
        }
        let file = fs::File::open(&path).context("opening Codex config.toml")?;
        let mut text = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)
            .context("reading Codex config.toml")?;
        ensure!(
            text.len() as u64 <= MAX_CONFIG_BYTES,
            "Codex config.toml exceeds 4 MiB"
        );
        let value = Arc::new(
            toml::from_str::<Config>(&text)
                .map_err(|_| anyhow!("cannot read sqlite_home from Codex config.toml"))?,
        );
        if let Some(stamp) = stamp {
            let mut cache = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.len() >= 64 {
                cache.clear();
            }
            cache.insert(
                path,
                CachedConfig {
                    stamp,
                    value: Arc::clone(&value),
                },
            );
        }
        Ok(Some(value))
    }
}

fn environment_path(value: &str) -> Option<&Path> {
    let value = value.trim();
    (!value.is_empty()).then(|| Path::new(value))
}

// Codex resolves settings against the file that defines them and environment paths against its cwd.
fn absolute_in(path: &Path, base: &Path) -> Result<PathBuf> {
    let expanded = if let Ok(relative) = path.strip_prefix("~") {
        directories::BaseDirs::new()
            .context("resolving the Codex SQLite home directory")?
            .home_dir()
            .join(relative)
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in std::path::absolute(expanded)?.components() {
        if component == Component::ParentDir {
            normalized.pop();
        } else {
            normalized.push(component);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_settings_override_the_environment_in_provider_order() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path().join("codex");
        let cwd = fixture.path().join("launch");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&cwd)?;
        let resolver = SqliteHome {
            environment: Some(PathBuf::from("original")),
            ..Default::default()
        };
        let mut launch = CodexLaunchOverrides::default();
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            cwd.join("original")
        );
        launch.environment = Some(EnvChange::Set {
            name: "CODEX_SQLITE_HOME".into(),
            value: "  alternate  ".into(),
        });
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            cwd.join("alternate")
        );
        launch.environment = Some(EnvChange::Remove("CODEX_SQLITE_HOME".into()));
        assert_eq!(resolver.resolve_in(&home, &cwd, &launch)?, home);
        fs::write(home.join("config.toml"), "sqlite_home = 'configured'")?;
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            home.join("configured")
        );
        let profile_file = home.join("work.config.toml");
        fs::write(&profile_file, "sqlite_home = 'named'")?;
        launch.profile = Some("work".into());
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            home.join("named")
        );
        let project = cwd.join(".codex");
        fs::create_dir(&project)?;
        fs::write(project.join("config.toml"), "sqlite_home = 'local'")?;
        fs::write(
            profile_file,
            format!(
                "sqlite_home = 'named'\n[projects.{}]\ntrust_level = 'trusted'",
                serde_json::to_string(&cwd)?
            ),
        )?;
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            project.join("local")
        );
        launch.sqlite_home = Some(PathBuf::from("explicit"));
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            cwd.join("explicit")
        );
        Ok(())
    }
}
