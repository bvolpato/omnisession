use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Result;

use super::{Config, ConfiguredPath, SqliteHome, Trust};

impl SqliteHome {
    pub(super) fn project_home(
        &self,
        home: &Path,
        cwd: &Path,
        user: &Config,
        launch_markers: Option<&[String]>,
    ) -> Result<Option<ConfiguredPath>> {
        if user.projects.is_empty() {
            return Ok(None);
        }
        let markers = launch_markers.or(user.project_root_markers.as_deref());
        let project = cwd
            .ancestors()
            .find(|dir| match markers {
                Some(markers) => markers.iter().any(|name| has_marker(dir, name)),
                None => has_marker(dir, ".git"),
            })
            .unwrap_or(cwd);
        let mut layers: Vec<_> = cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(project))
            .collect();
        layers.reverse();
        let root_trust = OnceLock::new();
        let mut selected = None;
        for directory in layers {
            let layer = directory.join(".codex");
            if !layer.is_dir() || same_file::is_same_file(&layer, home).unwrap_or(false) {
                continue;
            }
            let trust = user.trust(directory).or_else(|| {
                *root_trust.get_or_init(|| {
                    user.trust(project).or_else(|| {
                        let checkout = cwd.ancestors().find(|dir| has_marker(dir, ".git"))?;
                        user.trust(&repository_trust_root(checkout)?)
                    })
                })
            });
            if trust == Some(Trust::Trusted)
                && let Some(config) = self.configured(layer.join("config.toml"))?
                && let Some(path) = &config.sqlite_home
            {
                selected = Some(path.clone());
            }
        }
        Ok(selected)
    }
}

impl Config {
    fn trust(&self, directory: &Path) -> Option<Trust> {
        let canonical = directory.canonicalize().ok();
        if let Some(path) = canonical {
            let key = path.to_string_lossy();
            #[cfg(windows)]
            let key = if let Some(share) = key.strip_prefix(r"\\?\UNC\") {
                std::borrow::Cow::Owned(format!(r"\\{share}"))
            } else {
                std::borrow::Cow::Borrowed(key.strip_prefix(r"\\?\").unwrap_or(&key))
            };
            if let Some(trust) = self.trust_key(&key) {
                return Some(trust);
            }
        }
        self.trust_key(&directory.to_string_lossy())
    }

    fn trust_key(&self, key: &str) -> Option<Trust> {
        #[cfg(windows)]
        let lowercase = key.to_ascii_lowercase();
        #[cfg(windows)]
        let key = lowercase.as_str();
        if let Some(trust) = self
            .projects
            .get(key)
            .and_then(|project| project.trust_level)
        {
            return Some(trust);
        }
        #[cfg(windows)]
        return self
            .projects
            .iter()
            .filter(|(name, project)| {
                name.eq_ignore_ascii_case(key) && project.trust_level.is_some()
            })
            .min_by_key(|(name, _)| *name)
            .and_then(|(_, project)| project.trust_level);
        #[cfg(not(windows))]
        None
    }
}

fn has_marker(directory: &Path, name: &str) -> bool {
    let path = directory.join(name);
    fs::metadata(&path)
        .is_ok_and(|meta| name != ".git" || !meta.is_dir() || path.join("HEAD").exists())
}

// Codex checks the worktree registration before it applies trust from the main checkout.
fn repository_trust_root(checkout: &Path) -> Option<PathBuf> {
    let marker = checkout.join(".git");
    if marker.is_dir() {
        return Some(checkout.to_owned());
    }
    let administrative = git_pointer(&marker, true)?;
    if !fs::symlink_metadata(&administrative).ok()?.is_dir() {
        return None;
    }
    let canonical = administrative.canonicalize().ok()?;
    let worktrees = canonical.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common = worktrees.parent()?;
    let registered = git_pointer(&canonical.join("gitdir"), false)?;
    if registered.file_name()? != ".git"
        || registered.parent()?.canonicalize().ok()?.as_os_str()
            != checkout.canonicalize().ok()?.as_os_str()
        || git_pointer(&canonical.join("commondir"), false)?
            .canonicalize()
            .ok()?
            .as_os_str()
            != common.as_os_str()
    {
        return None;
    }
    let main = administrative.parent()?.parent()?.parent()?;
    let main_marker = main.join(".git");
    let main_admin = if main_marker.is_dir() {
        main_marker
    } else {
        git_pointer(&main_marker, true)?
    };
    (main_admin.canonicalize().ok()?.as_os_str() == common.as_os_str()).then(|| main.to_owned())
}

fn git_pointer(path: &Path, has_prefix: bool) -> Option<PathBuf> {
    const LIMIT: u64 = 64 * 1024;
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    let bytes = bytes.trim_ascii();
    let value = if has_prefix {
        bytes.strip_prefix(b"gitdir:")?.trim_ascii()
    } else {
        bytes
    };
    if value.is_empty() {
        return None;
    }
    #[cfg(unix)]
    let value = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(value);
    #[cfg(not(unix))]
    let value = std::str::from_utf8(value).ok()?;
    Some(path.parent()?.join(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::rollout_identity::sqlite_home::Project;
    use std::process::Command;

    #[test]
    fn a_registered_worktree_inherits_project_sqlite_home_but_a_copied_pointer_does_not()
    -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        let git = |args: &[&str]| -> Result<()> {
            let mut cmd = Command::new("git");
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(key);
                }
            }
            let result = cmd
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", root.join("empty-config"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()?;
            anyhow::ensure!(
                result.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(())
        };
        git(&["init", "--template=", "main"])?;
        git(&[
            "-C",
            "main",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ])?;
        git(&["-C", "main", "worktree", "add", "--detach", "../linked"])?;
        let home = root.join("codex");
        let main = root.join("main");
        let linked = root.join("linked");
        let copied = root.join("copied");
        fs::create_dir(&home)?;
        for checkout in [&linked, &copied] {
            fs::create_dir_all(checkout.join(".codex"))?;
            fs::write(
                checkout.join(".codex/config.toml"),
                "sqlite_home = '../state'",
            )?;
        }
        fs::copy(linked.join(".git"), copied.join(".git"))?;
        let mut user = Config::default();
        user.projects.insert(
            main.to_string_lossy().into_owned(),
            Project {
                trust_level: Some(Trust::Trusted),
            },
        );
        let selector = SqliteHome::default();
        assert_eq!(
            selector.project_home(&home, &linked, &user, None)?,
            Some(ConfiguredPath::Path(linked.join("state")))
        );
        assert_eq!(selector.project_home(&home, &copied, &user, None)?, None);
        Ok(())
    }
}
