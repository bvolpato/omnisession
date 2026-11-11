use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

use super::{Config, ConfiguredPath, SqliteHome};

mod macos;
pub(super) use macos::{MacPreferences, Preference};

impl SqliteHome {
    pub(super) fn mac_config(&self, home: &Path, key: Preference) -> Result<Option<Arc<Config>>> {
        match &self.macos_preferences {
            Some(prefs) => prefs.config(home, key).with_context(|| format!(
                "Omni cannot verify the state database from the managed macOS Codex {}. Open the thread directly in Codex.", key.name()
            )),
            None => Ok(None),
        }
    }

    pub(super) fn legacy_managed_config(&self) -> Result<Option<Arc<Config>>> {
        if cfg!(unix)
            && let Some(system) = &self.system_config
        {
            return self
                .configured(system.with_file_name("managed_config.toml"))
                .context("cannot verify local Codex managed_config.toml");
        }
        Ok(None)
    }

    pub(super) fn required_home(&self, home: &Path) -> Result<Option<PathBuf>> {
        let managed = self.mac_config(home, Preference::Requirements)?;
        let file = self.file_requirements()?;
        if let Some(path) = managed.as_ref().and_then(|layer| layer.sqlite_home.clone()) {
            return Ok(Some(path.into_path()?));
        }
        anyhow::ensure!(
            !managed
                .iter()
                .chain(&file)
                .any(|layer| layer.requires_server),
            "Managed Codex requirements select a configuration server that may change the state database. Open the thread directly in Codex."
        );
        file.and_then(|config| config.sqlite_home.clone())
            .map(ConfiguredPath::into_path)
            .transpose()
    }

    fn file_requirements(&self) -> Result<Option<Arc<Config>>> {
        let Some(system) = &self.system_config else {
            return Ok(None);
        };
        let path = system.with_file_name("requirements.toml");
        #[cfg(windows)]
        if self.use_system_registry {
            check_requirements_path(&path)?;
        }
        self.configured(path)
            .context("cannot verify sqlite_home in local Codex requirements.toml")
    }
}

#[cfg(windows)]
fn check_requirements_path(default: &std::path::Path) -> Result<()> {
    use winreg::{
        RegKey,
        enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
    };

    let configured = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Policies\OpenAI\Codex",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<std::ffi::OsString, _>("RequirementsPath"));
    if let Ok(path) = configured {
        anyhow::ensure!(
            std::path::Path::new(&path) == default,
            "Codex uses a custom RequirementsPath. Open the thread directly in Codex to use this policy."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::CodexLaunchOverrides;
    use base64::{Engine, engine::general_purpose::STANDARD};

    #[test]
    fn managed_preferences_change_only_the_settings_that_select_local_storage() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path();
        fs::write(home.join("config.toml"), "sqlite_home = 'user-db'")?;
        let mut selector = SqliteHome {
            macos_preferences: Some(MacPreferences::new(|key| {
                Ok(Some(STANDARD.encode(match key {
                    Preference::Config => "model = 'example'\n[tui]\nanimations = false",
                    Preference::Requirements => "allowed_approval_policies = ['on-request']",
                })))
            })),
            ..SqliteHome::default()
        };
        assert_eq!(selector.resolve(home)?, home.join("user-db"));
        selector.macos_preferences = Some(MacPreferences::new(|key| {
            Ok(Some(STANDARD.encode(match key {
                Preference::Config => "sqlite_home = 'configured'",
                Preference::Requirements => "sqlite_home = 'required'",
            })))
        }));
        assert_eq!(selector.resolve(home)?, home.join("required"));
        let other_home = home.join("other");
        assert_eq!(selector.resolve(&other_home)?, other_home.join("required"));
        selector.macos_preferences = Some(MacPreferences::new(|key| {
            Ok(matches!(key, Preference::Requirements)
                .then(|| STANDARD.encode("[config_server]\nid = 'private'")))
        }));
        let error = format!("{:#}", selector.resolve(home).unwrap_err());
        assert!(error.contains("configuration server"));
        assert!(!error.contains("private"));
        Ok(())
    }

    #[test]
    fn a_local_sqlite_requirement_overrides_config_flags_and_environment() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let system = root.join("system");
        let home = root.join("home");
        fs::create_dir(&system)?;
        fs::create_dir(&home)?;
        fs::write(home.join("config.toml"), "sqlite_home = 'from-user'")?;
        let selector = SqliteHome {
            system_config: Some(system.join("config.toml")),
            environment: Some(root.join("from-env")),
            ..SqliteHome::default()
        };
        let launch = CodexLaunchOverrides {
            sqlite_home: Some("from-cli".into()),
            ..CodexLaunchOverrides::default()
        };
        let required = system.join("requirements.toml");
        fs::write(&required, "allowed_approval_policies = ['on-request']")?;
        assert_eq!(
            selector.resolve_in(&home, root, &launch)?,
            root.join("from-cli")
        );
        #[cfg(unix)]
        {
            fs::write(
                system.join("managed_config.toml"),
                "sqlite_home = 'from-managed'",
            )?;
            assert_eq!(
                selector.resolve_in(&home, root, &launch)?,
                system.join("from-managed")
            );
        }
        fs::write(&required, "sqlite_home = '../from-policy'")?;
        assert_eq!(
            selector.resolve_in(&home, root, &launch)?,
            root.join("from-policy")
        );
        for invalid in ["sqlite_home = 123", "sqlite_home = ["] {
            fs::write(&required, invalid)?;
            assert!(selector.resolve_in(&home, root, &launch).is_err());
        }
        Ok(())
    }
}
