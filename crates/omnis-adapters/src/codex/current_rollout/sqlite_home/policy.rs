use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};

use super::{Config, ConfiguredPath, SqliteHome};

#[cfg(target_os = "macos")]
pub(super) fn macos_preference_is_forced(domain: &str, key: &str) -> bool {
    use objc2_core_foundation::{CFPreferencesAppValueIsForced, CFString};

    CFPreferencesAppValueIsForced(&CFString::from_str(key), &CFString::from_str(domain))
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn check_macos_preferences(is_forced: impl Fn(&str, &str) -> bool) -> Result<()> {
    // Either forced preference can change the selected state database. Leave its contents in Codex.
    anyhow::ensure!(
        !["config_toml_base64", "requirements_toml_base64"]
            .iter()
            .any(|key| is_forced("com.openai.codex", key)),
        "Omni cannot verify the state database under this managed macOS Codex policy. Open the thread directly in Codex."
    );
    Ok(())
}

impl SqliteHome {
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

    pub(super) fn required_home(&self) -> Result<Option<PathBuf>> {
        let Some(system) = &self.system_config else {
            return Ok(None);
        };
        let path = system.with_file_name("requirements.toml");
        #[cfg(windows)]
        if self.use_system_registry {
            check_requirements_path(&path)?;
        }
        self.configured(path)
            .and_then(|config| {
                config
                    .and_then(|config| config.sqlite_home.clone())
                    .map(ConfiguredPath::into_path)
                    .transpose()
            })
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

    #[test]
    fn forced_macos_preferences_prevent_selection_from_lower_config() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path();
        fs::write(home.join("config.toml"), "sqlite_home = 'user-db'")?;
        let mut selector = SqliteHome {
            macos_preference_is_forced: Some(|_, _| false),
            ..SqliteHome::default()
        };
        assert_eq!(selector.resolve(home)?, home.join("user-db"));
        for forced in [
            (|domain, key| domain == "com.openai.codex" && key == "config_toml_base64")
                as fn(&str, &str) -> bool,
            |domain, key| domain == "com.openai.codex" && key == "requirements_toml_base64",
        ] {
            selector.macos_preference_is_forced = Some(forced);
            let error = selector.resolve(home).unwrap_err().to_string();
            assert!(error.contains("Open the thread directly in Codex"));
        }
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
