use std::{
    env,
    path::{Component, Path, PathBuf},
};

#[must_use]
pub fn oh_my_pi_sessions_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("OMP_SESSION_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(root));
    }
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    let config = env::var_os("PI_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(".omp"), PathBuf::from);
    let profile = env::var("OMP_PROFILE")
        .ok()
        .or_else(|| env::var("PI_PROFILE").ok());
    let legacy_agent = env::var("PI_PROFILE")
        .ok()
        .map(|profile| profile.trim().to_owned())
        .filter(|profile| profile != "default" && valid_profile(profile))
        .map(|profile| {
            home.join(&config)
                .join("profiles")
                .join(profile)
                .join("agent")
        });
    let agent = env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|agent| Some(agent.as_path()) != legacy_agent.as_deref());
    resolve_root(
        &home,
        &config,
        profile.as_deref(),
        agent.as_deref(),
        env::var_os("XDG_DATA_HOME").map(PathBuf::from).as_deref(),
    )
}

fn resolve_root(
    home: &Path,
    config: &Path,
    profile: Option<&str>,
    agent: Option<&Path>,
    xdg: Option<&Path>,
) -> Option<PathBuf> {
    if config
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let profile = profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty() && *profile != "default");
    if profile.is_some_and(|profile| !valid_profile(profile)) {
        return None;
    }
    let mut root = home.join(config);
    if let Some(profile) = profile {
        root = root.join("profiles").join(profile);
    }
    let default_agent = root.join("agent");
    let agent = agent
        .filter(|agent| profile.is_none() && !agent.as_os_str().is_empty())
        .unwrap_or(&default_agent);
    if cfg!(any(target_os = "linux", target_os = "macos"))
        && agent == default_agent
        && let Some(xdg) = xdg.filter(|xdg| !xdg.as_os_str().is_empty())
    {
        if !xdg.is_absolute() {
            return None;
        }
        let mut data = xdg.join("omp");
        if let Some(profile) = profile {
            data = data.join("profiles").join(profile);
        }
        if data.is_dir() {
            return Some(data.join("sessions"));
        }
    }
    Some(agent.join("sessions"))
}

fn valid_profile(profile: &str) -> bool {
    let basename = profile.split('.').next().unwrap_or_default();
    let reserved = matches!(basename, "con" | "prn" | "aux" | "nul")
        || (basename.len() == 4
            && (basename.starts_with("com") || basename.starts_with("lpt"))
            && basename.as_bytes()[3].is_ascii_digit());
    profile.len() <= 64
        && !profile.ends_with('.')
        && !reserved
        && profile
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && profile.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_selected_profile_and_migrated_xdg_root() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let xdg = temp.path().join("data");
        let config = Path::new(".omp");
        std::fs::create_dir_all(home.join(".local/share/omp")).unwrap();
        assert_eq!(
            resolve_root(&home, config, None, None, None),
            Some(home.join(".omp/agent/sessions"))
        );
        assert_eq!(
            resolve_root(&home, config, None, None, Some(Path::new("relative"))),
            None
        );
        assert_eq!(
            resolve_root(&home, config, None, None, Some(&xdg)),
            Some(home.join(".omp/agent/sessions"))
        );
        std::fs::create_dir_all(xdg.join("omp")).unwrap();
        assert_eq!(
            resolve_root(&home, config, None, None, Some(&xdg)),
            Some(xdg.join("omp/sessions"))
        );
        assert_eq!(
            resolve_root(&home, config, Some("work"), None, Some(&xdg)),
            Some(home.join(".omp/profiles/work/agent/sessions"))
        );
        std::fs::create_dir_all(xdg.join("omp/profiles/work")).unwrap();
        assert_eq!(
            resolve_root(&home, config, Some("work"), None, Some(&xdg)),
            Some(xdg.join("omp/profiles/work/sessions"))
        );
        let custom = temp.path().join("custom");
        assert_eq!(
            resolve_root(&home, config, None, Some(&custom), Some(&xdg)),
            Some(custom.join("sessions"))
        );
        for profile in ["../wrong", "Wrong", "con.txt", "trailing."] {
            assert_eq!(resolve_root(&home, config, Some(profile), None, None), None);
        }
    }
}
