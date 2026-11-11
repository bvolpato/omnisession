mod policy;
mod project;

use std::{
    collections::HashMap,
    env, fs,
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

use crate::{CodexLaunchOverrides, EnvChange};

use super::super::rollout_cache::RolloutStamp;

#[derive(Debug, Default)]
pub(super) struct SqliteHome {
    environment: Option<PathBuf>,
    source_expands_tilde: OnceLock<Option<bool>>,
    system_config: Option<PathBuf>,
    macos_preferences: Option<policy::MacPreferences>,
    #[cfg(windows)]
    use_system_registry: bool,
    config: Mutex<HashMap<PathBuf, CachedConfig>>,
}

#[derive(Debug)]
struct CachedConfig {
    stamp: RolloutStamp,
    value: Arc<Config>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Config {
    sqlite_home: Option<ConfiguredPath>,
    #[serde(skip)]
    requires_server: bool,
    #[serde(default)]
    projects: HashMap<String, Project>,
    project_root_markers: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct StorageRequirements {
    sqlite_home: Option<ConfiguredPath>,
    config_server: Option<serde::de::IgnoredAny>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum ConfiguredPath {
    Path(PathBuf),
    Invalid(serde::de::IgnoredAny),
}

impl ConfiguredPath {
    fn into_path(self) -> Result<PathBuf> {
        match self {
            Self::Path(path) => Ok(path),
            Self::Invalid(_) => Err(anyhow!("Codex sqlite_home must be a path")),
        }
    }
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
            system_config: Some(system_config_path()),
            #[cfg(target_os = "macos")]
            macos_preferences: Some(policy::MacPreferences::system()),
            #[cfg(windows)]
            use_system_registry: true,
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
        self.resolve_with(home, cwd, overrides, &|path, literal| {
            // Without a state database either spelling uses filesystem discovery.
            if absolute_in(path, cwd).is_ok_and(|expanded| [literal, &expanded].iter().all(|root| {
                root.join("state_5.sqlite")
                    .symlink_metadata()
                    .is_err_and(|error| error.kind() == ErrorKind::NotFound)
            })) {
                return Ok(true);
            }
            self.source_expands_tilde.get_or_init(source_tilde_behavior)
                .context("Cannot verify how the installed Codex resolves a leading tilde in CODEX_SQLITE_HOME. Set CODEX_SQLITE_HOME to an absolute path")
        })
    }

    pub(super) fn resolve_for_launch(
        &self,
        home: &Path,
        cwd: &Path,
        overrides: &CodexLaunchOverrides,
        expand_env_tilde: &dyn Fn() -> Result<bool>,
    ) -> Result<PathBuf> {
        self.resolve_with(home, cwd, overrides, &|_, _| expand_env_tilde())
    }

    fn resolve_with(
        &self,
        home: &Path,
        cwd: &Path,
        overrides: &CodexLaunchOverrides,
        expand_env_tilde: &dyn Fn(&Path, &Path) -> Result<bool>,
    ) -> Result<PathBuf> {
        let managed = self.legacy_managed_config()?;
        let managed = merge_settings(managed, self.mac_config(home, policy::Preference::Config)?);
        let settings = merge_settings(
            self.settings(home, overrides.profile.as_deref())?,
            managed.clone(),
        );
        let markers = managed
            .as_ref()
            .and_then(|config| config.project_root_markers.as_deref())
            .or(overrides.project_root_markers.as_deref());
        let project = if let Some(settings) = &settings {
            self.project_home(home, cwd, settings, markers)?
        } else {
            None
        };
        let configured = if let Some(path) = managed
            .as_ref()
            .and_then(|config| config.sqlite_home.clone())
        {
            Some(path.into_path()?)
        } else if let Some(path) = &overrides.sqlite_home {
            Some(absolute_in(path, cwd)?)
        } else {
            project
                .or_else(|| settings.as_ref()?.sqlite_home.clone())
                .map(ConfiguredPath::into_path)
                .transpose()?
        };
        if let Some(path) = self.required_home(home)?.or(configured) {
            return Ok(path);
        }
        let environment = match &overrides.environment {
            Some(EnvChange::Set { value, .. }) => environment_path(value),
            Some(EnvChange::Remove(_)) => None,
            None => self.environment.as_deref(),
        };
        if let Some(path) = environment {
            if path.starts_with("~") {
                let literal = std::path::absolute(cwd)?.join(path);
                match expand_env_tilde(path, &literal) {
                    Ok(false) => return Ok(literal),
                    Ok(true) => {}
                    Err(error) => {
                        let expanded = absolute_in(path, cwd)?;
                        if same_existing_directory(&literal, &expanded) {
                            return Ok(expanded);
                        }
                        return Err(error);
                    }
                }
            }
            return absolute_in(path, cwd);
        }
        Ok(home.to_owned())
    }

    fn settings(&self, home: &Path, profile: Option<&str>) -> Result<Option<Arc<Config>>> {
        let system = self
            .system_config
            .as_ref()
            .map(|path| self.configured(path.clone()))
            .transpose()?
            .flatten();
        let base = merge_settings(system, self.configured(home.join("config.toml"))?);
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
        Ok(merge_settings(
            base,
            self.configured(home.join(format!("{name}.config.toml")))?,
        ))
    }

    fn configured(&self, path: PathBuf) -> Result<Option<Arc<Config>>> {
        const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading Codex config.toml metadata"),
        };
        let file_path = if metadata.is_symlink() {
            let target = match path.canonicalize() {
                Ok(target) => target,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("resolving Codex config.toml"),
            };
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
            path.clone()
        };
        let metadata = fs::metadata(&file_path).context("checking Codex config.toml")?;
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
        let file = fs::File::open(&file_path).context("opening Codex config.toml")?;
        let mut text = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)
            .context("reading Codex config.toml")?;
        ensure!(
            text.len() as u64 <= MAX_CONFIG_BYTES,
            "Codex config.toml exceeds 4 MiB"
        );
        let value = Config::parse(
            &text,
            path.parent()
                .context("Codex config.toml has no parent directory")?,
            path.file_name()
                .is_some_and(|name| name == "requirements.toml"),
        )
        .context("cannot read sqlite_home from Codex config.toml")?;
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

impl Config {
    fn parse(text: &str, directory: &Path, requirements: bool) -> Result<Arc<Self>> {
        // Parser diagnostics include source lines. Return only the class of error.
        let mut config = if requirements {
            toml::from_str::<StorageRequirements>(text).map(|fields| Self {
                sqlite_home: fields.sqlite_home,
                requires_server: fields.config_server.is_some(),
                ..Self::default()
            })
        } else {
            toml::from_str(text)
        }
        .map_err(|_| anyhow!("invalid Codex settings"))?;
        if let Some(ConfiguredPath::Path(value)) = &mut config.sqlite_home {
            *value = absolute_in(value, directory)?;
        }
        Ok(Arc::new(config))
    }
}

fn merge_settings(lower: Option<Arc<Config>>, upper: Option<Arc<Config>>) -> Option<Arc<Config>> {
    let (lower, upper) = match (lower, upper) {
        (Some(lower), Some(upper)) => (lower, upper),
        (lower, upper) => return lower.or(upper),
    };
    let mut merged = Arc::unwrap_or_clone(lower);
    if upper.sqlite_home.is_some() {
        merged.sqlite_home.clone_from(&upper.sqlite_home);
    }
    if upper.project_root_markers.is_some() {
        merged
            .project_root_markers
            .clone_from(&upper.project_root_markers);
    }
    for (name, project) in &upper.projects {
        if project.trust_level.is_some() {
            merged.projects.insert(name.clone(), *project);
        }
    }
    Some(Arc::new(merged))
}

#[cfg(unix)]
fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/codex/config.toml")
}

#[cfg(windows)]
fn system_config_path() -> PathBuf {
    known_folders::get_known_folder_path(known_folders::KnownFolder::ProgramData)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("OpenAI")
        .join("Codex")
        .join("config.toml")
}

fn source_tilde_behavior() -> Option<bool> {
    let executable = crate::support::provider_executable(omnis_ir::Provider::Codex)?;
    let mut command = crate::provider_process(&executable).ok()?;
    // Codex can install helper shims even for --version. Keep them out of the session store.
    let home = tempfile::tempdir().ok()?;
    command
        .env("CODEX_HOME", home.path())
        .env_remove("CODEX_SQLITE_HOME")
        .arg("--version")
        .stdin(std::process::Stdio::null());
    let output = crate::bounded_provider_line_output(
        &mut command,
        std::time::Duration::from_secs(15),
        8192,
        |line| line.trim_start().starts_with("codex-cli "),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let version = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("codex-cli "))?;
    crate::CodexAdapter::expands_sqlite_env_tilde(version.split_whitespace().next()?)
}

fn same_existing_directory(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .is_ok_and(|left| left.is_dir() && fs::canonicalize(right).is_ok_and(|right| right == left))
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
        let system = fixture.path().join("system");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&cwd)?;
        fs::create_dir(&system)?;
        let resolver = SqliteHome {
            environment: Some(PathBuf::from("original")),
            system_config: Some(system.join("config.toml")),
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
        launch.environment = Some(EnvChange::Set {
            name: "CODEX_SQLITE_HOME".into(),
            value: "~/state".into(),
        });
        assert_eq!(
            resolver.resolve_for_launch(&home, &cwd, &launch, &|| Ok(false))?,
            cwd.join("~").join("state")
        );
        assert_eq!(
            resolver.resolve_for_launch(&home, &cwd, &launch, &|| Ok(true))?,
            absolute_in(Path::new("~/state"), &cwd)?
        );
        let unknown = || Err(anyhow!("unknown Codex version"));
        assert!(
            resolver
                .resolve_for_launch(&home, &cwd, &launch, &unknown)
                .is_err()
        );
        launch.sqlite_home = Some(home.join("explicit"));
        assert_eq!(
            resolver.resolve_for_launch(&home, &cwd, &launch, &unknown)?,
            home.join("explicit")
        );
        launch.sqlite_home = None;
        launch.environment = Some(EnvChange::Remove("CODEX_SQLITE_HOME".into()));
        assert_eq!(resolver.resolve_in(&home, &cwd, &launch)?, home);
        fs::write(
            system.join("config.toml"),
            "sqlite_home = '../managed/state'",
        )?;
        launch.environment = None;
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            fixture.path().join("managed/state")
        );
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

    #[test]
    fn system_project_trust_and_markers_apply_before_user_overrides() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        let home = root.join("home");
        let project = root.join("project");
        let cwd = project.join("subdirectory");
        let layer = project.join(".codex");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&cwd)?;
        fs::create_dir(&layer)?;
        fs::write(project.join("workspace.marker"), "")?;
        fs::write(layer.join("config.toml"), "sqlite_home = 'selected'")?;
        let system = root.join("system.toml");
        fs::write(
            &system,
            format!(
                "sqlite_home = {}\nproject_root_markers = ['workspace.marker']\n[projects.{}]\ntrust_level = 'trusted'",
                serde_json::to_string(&root.join("absolute-state"))?,
                serde_json::to_string(&project)?,
            ),
        )?;
        let resolver = SqliteHome {
            system_config: Some(system),
            ..Default::default()
        };
        let launch = CodexLaunchOverrides::default();
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            layer.join("selected")
        );
        fs::write(
            home.join("config.toml"),
            format!(
                "[projects.{}]\ntrust_level = 'untrusted'",
                serde_json::to_string(&project)?
            ),
        )?;
        assert_eq!(
            resolver.resolve_in(&home, &cwd, &launch)?,
            root.join("absolute-state")
        );
        Ok(())
    }

    #[test]
    fn an_invalid_system_layer_prevents_guessing_a_database() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let system = root.join("system.toml");
        let resolver = SqliteHome {
            system_config: Some(system.clone()),
            environment: Some(root.join("env-state")),
            ..Default::default()
        };
        let launch = CodexLaunchOverrides {
            sqlite_home: Some(root.join("cli-state")),
            ..Default::default()
        };
        fs::write(&system, "sqlite_home = [")?;
        assert!(resolver.resolve_in(root, root, &launch).is_err());
        fs::write(&system, "sqlite_home = 123")?;
        assert!(
            resolver
                .resolve_in(root, root, &CodexLaunchOverrides::default())
                .is_err()
        );
        fs::write(root.join("config.toml"), "sqlite_home = 'user-state'")?;
        assert_eq!(
            resolver.resolve_in(root, root, &CodexLaunchOverrides::default())?,
            root.join("user-state")
        );
        fs::remove_file(&system)?;
        fs::create_dir(&system)?;
        assert!(
            resolver
                .resolve_in(root, root, &CodexLaunchOverrides::default())
                .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_system_file_cannot_select_the_environment_database() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let system = root.join("system.toml");
        fs::write(&system, "sqlite_home = 'system-state'")?;
        fs::set_permissions(&system, fs::Permissions::from_mode(0o000))?;
        if fs::File::open(&system).is_err_and(|error| error.kind() == ErrorKind::PermissionDenied) {
            let resolver = SqliteHome {
                system_config: Some(system),
                environment: Some(root.join("env-state")),
                ..Default::default()
            };
            assert!(
                resolver
                    .resolve_in(root, root, &CodexLaunchOverrides::default())
                    .is_err()
            );
        }
        Ok(())
    }
}
