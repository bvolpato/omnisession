//! Provider executable discovery and subprocess preparation.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use omnis_ir::Provider;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use directories::BaseDirs;

use super::provider_compatibility::{Capability, supports_capability};
use super::{PROVIDERS, cursor_ide_import, state_root};

// Older releases installed provider aliases here. Exclude any leftovers from real-provider lookup.
fn legacy_alias_directory() -> Result<PathBuf> {
    Ok(state_root()
        .context("resolving OmniSession state")?
        .join("shims"))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn provider_override(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Claude => Some("OMNI_CLAUDE_BIN"),
        Provider::Codex => Some("OMNI_CODEX_BIN"),
        Provider::OpenCode => Some("OMNI_OPENCODE_BIN"),
        Provider::Grok => Some("OMNI_GROK_BIN"),
        Provider::Hermes => Some("OMNI_HERMES_BIN"),
        Provider::Antigravity => Some("OMNI_ANTIGRAVITY_BIN"),
        Provider::Pi => Some("OMNI_PI_BIN"),
        Provider::OhMyPi => Some("OMNI_OMP_BIN"),
        Provider::CursorCli => Some("OMNI_CURSOR_AGENT_BIN"),
        Provider::AntigravityIde
        | Provider::AntigravityDesktop
        | Provider::CursorIde
        | Provider::GenericAcp
        | Provider::Imported => None,
    }
}

fn resolve_real_binary(provider: Provider, excluded_directory: &Path) -> Result<PathBuf> {
    let command_name = provider
        .command()
        .ok_or_else(|| anyhow!("provider `{provider}` has no command"))?;
    let current_exe = env::current_exe()
        .context("resolving current OmniSession executable")?
        .canonicalize()
        .context("canonicalizing current OmniSession executable")?;
    if let Some(variable) = provider_override(provider)
        && let Some(override_path) = env::var_os(variable).filter(|value| !value.is_empty())
    {
        let override_path = PathBuf::from(override_path);
        if !override_path.is_absolute() {
            bail!("{variable} must contain an absolute executable path");
        }
        let binary = validate_real_binary(&override_path, excluded_directory, &current_exe)
            .with_context(|| format!("validating {variable}"))?;
        if provider == Provider::CursorCli && !cursor_agent_binary_name_matches(&binary) {
            bail!("{variable} does not identify a Cursor Agent executable");
        }
        return Ok(binary);
    }

    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    if provider == Provider::CursorCli {
        return resolve_cursor_agent_binary(&path, excluded_directory, &current_exe);
    }
    for directory in env::split_paths(&path) {
        if same_path(&directory, excluded_directory) {
            continue;
        }
        for candidate in executable_candidates(&directory, command_name) {
            if let Ok(candidate) =
                validate_real_binary(&candidate, excluded_directory, &current_exe)
            {
                return Ok(candidate);
            }
        }
    }
    let override_name = provider_override(provider).expect("provider override");
    bail!("real `{command_name}` not found; set {override_name} to its absolute path")
}

fn resolve_cursor_agent_binary(
    path: &OsStr,
    excluded_directory: &Path,
    current_exe: &Path,
) -> Result<PathBuf> {
    for command_name in ["agent", "cursor-agent"] {
        for directory in env::split_paths(path) {
            if same_path(&directory, excluded_directory) {
                continue;
            }
            for candidate in executable_candidates(&directory, command_name) {
                let Ok(candidate) =
                    validate_real_binary(&candidate, excluded_directory, current_exe)
                else {
                    continue;
                };
                if cursor_agent_binary_name_matches(&candidate) {
                    return Ok(candidate);
                }
            }
        }
    }
    bail!("Cursor Agent not found; set OMNI_CURSOR_AGENT_BIN to its absolute path")
}

fn cursor_agent_binary_name_matches(binary: &Path) -> bool {
    let Some(name) = binary.file_stem().and_then(OsStr::to_str) else {
        return false;
    };
    if name.eq_ignore_ascii_case("cursor-agent") {
        return true;
    }
    cfg!(windows)
        && name.eq_ignore_ascii_case("agent")
        && binary
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .is_some_and(|parent| parent.eq_ignore_ascii_case("cursor-agent"))
}

pub(super) fn resolved_provider_binary(provider: Provider) -> Result<PathBuf> {
    resolve_real_binary(provider, &legacy_alias_directory()?)
}

pub(super) fn cursor_ide_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("OMNI_CURSOR_IDE_BIN").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("OMNI_CURSOR_IDE_BIN must contain an absolute executable path");
        }
        if !is_executable(&path) {
            bail!("OMNI_CURSOR_IDE_BIN is not executable");
        }
        return validate_cursor_ide_binary(&path)
            .context("OMNI_CURSOR_IDE_BIN does not identify a Cursor desktop application");
    }
    cursor_ide_binary_candidate()
        .context("Cursor IDE binary not found; set OMNI_CURSOR_IDE_BIN to its executable path")
}

fn cursor_ide_binary_candidate() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home = BaseDirs::new()?;
    let mut candidates = Vec::new();

    #[cfg(target_os = "linux")]
    {
        let applications = home.home_dir().join("Applications");
        push_unique_path(&mut candidates, applications.join("Cursor.AppImage"));
        let mut appimages = fs::read_dir(&applications)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("Cursor-") && name.ends_with(".AppImage"))
            })
            .collect::<Vec<_>>();
        appimages.sort_by(|left, right| right.cmp(left));
        candidates.extend(appimages);
        push_unique_path(&mut candidates, PathBuf::from("/usr/bin/cursor"));
        push_unique_path(&mut candidates, PathBuf::from("/usr/local/bin/cursor"));
    }

    #[cfg(target_os = "macos")]
    {
        for applications in [
            home.home_dir().join("Applications"),
            PathBuf::from("/Applications"),
        ] {
            push_unique_path(
                &mut candidates,
                applications.join("Cursor.app/Contents/MacOS/Cursor"),
            );
        }
    }

    #[cfg(windows)]
    {
        if let Some(root) = env::var_os("LOCALAPPDATA") {
            push_unique_path(
                &mut candidates,
                PathBuf::from(root).join("Programs/cursor/Cursor.exe"),
            );
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = env::var_os(variable) {
                push_unique_path(
                    &mut candidates,
                    PathBuf::from(root).join("Cursor/Cursor.exe"),
                );
            }
        }
    }

    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            for name in ["cursor", "Cursor"] {
                for candidate in executable_candidates(&directory, name) {
                    push_unique_path(&mut candidates, candidate);
                }
            }
        }
    }
    select_cursor_ide_binary(candidates)
}

fn select_cursor_ide_binary(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let candidates = candidates
        .into_iter()
        .filter_map(|path| validate_cursor_ide_binary(&path).ok())
        .collect::<Vec<_>>();
    candidates
        .iter()
        .find(|path| cursor_ide_import::ensure_supported(path).is_ok())
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn validate_cursor_ide_binary(path: &Path) -> Result<PathBuf> {
    if !is_executable(path) {
        bail!("`{}` is not executable", path.display());
    }
    let binary =
        fs::canonicalize(path).with_context(|| format!("canonicalizing `{}`", path.display()))?;
    cursor_ide_import::validate_desktop_identity(&binary)
        .with_context(|| format!("identifying `{}` as Cursor IDE", path.display()))?;
    Ok(binary)
}

pub(super) fn cursor_ide_cross_import_ready(binary: &Path) -> bool {
    cursor_ide_import::ensure_supported(binary).is_ok()
}

fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.contains(&candidate) {
        paths.push(candidate);
    }
}

pub(super) fn runnable_target_providers() -> Vec<Provider> {
    let Ok(excluded_directory) = legacy_alias_directory() else {
        return Vec::new();
    };
    PROVIDERS
        .into_iter()
        .filter(|provider| {
            let capable = [
                Capability::CleanStart,
                Capability::SameProviderResume,
                Capability::CrossProviderImport,
            ]
            .into_iter()
            .any(|capability| supports_capability(*provider, capability));
            capable
                && if *provider == Provider::CursorIde {
                    cursor_ide_binary().is_ok_and(|binary| cursor_ide_cross_import_ready(&binary))
                } else {
                    resolve_real_binary(*provider, &excluded_directory).is_ok()
                }
        })
        .collect()
}

fn validate_real_binary(
    candidate: &Path,
    excluded_directory: &Path,
    current_exe: &Path,
) -> Result<PathBuf> {
    if !is_executable(candidate) {
        bail!("`{}` is not executable", candidate.display());
    }
    let candidate = fs::canonicalize(candidate)
        .with_context(|| format!("canonicalizing `{}`", candidate.display()))?;
    let is_current_executable = candidate == current_exe;
    #[cfg(windows)]
    let is_current_executable =
        is_current_executable || same_file::is_same_file(&candidate, current_exe).unwrap_or(false);
    if is_current_executable || candidate.starts_with(canonical_or_original(excluded_directory)) {
        bail!(
            "`{}` resolves to an OmniSession executable or legacy alias",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn same_path(left: &Path, right: &Path) -> bool {
    canonical_or_original(left) == canonical_or_original(right)
}

pub(super) fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    #[cfg(not(windows))]
    {
        vec![directory.join(name)]
    }
    #[cfg(windows)]
    {
        windows_executable_candidates(directory, name, env::var_os("PATHEXT").as_deref())
    }
}

#[cfg(any(test, windows))]
fn windows_executable_candidates(
    directory: &Path,
    name: &str,
    path_extensions: Option<&OsStr>,
) -> Vec<PathBuf> {
    let path_extensions = path_extensions
        .filter(|extensions| !extensions.is_empty())
        .map_or_else(|| ".COM;.EXE;.BAT;.CMD".into(), OsStr::to_string_lossy);
    path_extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| directory.join(format!("{name}{extension}")))
        .collect()
}

#[cfg(unix)]
pub(super) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(super) fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Returns the exit code a shell reports for `status`: its code, or 128 plus its ending signal.
pub(super) fn shell_exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

pub(super) use omnis_adapters::provider_process;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents).expect("write executable fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("make fixture executable");
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[cfg(windows)]
    #[test]
    fn cursor_agent_identity_accepts_only_windows_installer_alias_directory() {
        assert!(cursor_agent_binary_name_matches(Path::new(
            r"C:\Users\developer\AppData\Local\cursor-agent\agent.exe"
        )));
        assert!(!cursor_agent_binary_name_matches(Path::new(
            r"C:\Users\developer\bin\agent.exe"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn cursor_agent_resolution_accepts_primary_agent_only() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary Cursor Agent installation");
        let bin = temporary.path().join("bin");
        let version = temporary.path().join("versions/current");
        let excluded_directory = temporary.path().join("legacy-aliases");
        fs::create_dir_all(&bin).expect("binary directory");
        fs::create_dir_all(&version).expect("version directory");
        fs::create_dir_all(&excluded_directory).expect("excluded directory");
        let cursor_agent = version.join("cursor-agent");
        let launch_marker = temporary.path().join("agent-launched");
        write_executable(
            &cursor_agent,
            &format!("#!/bin/sh\n: >{}\n", shell_quote(&launch_marker)),
        );
        symlink(&cursor_agent, bin.join("agent")).expect("primary agent alias");
        let current_exe = temporary.path().join("omni");
        write_executable(&current_exe, "#!/bin/sh\nexit 0\n");

        let path = env::join_paths([&bin]).expect("fixture PATH");
        let resolved = resolve_cursor_agent_binary(&path, &excluded_directory, &current_exe)
            .expect("resolve primary agent");

        assert_eq!(resolved, cursor_agent.canonicalize().expect("Cursor Agent"));
        assert!(!launch_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cursor_agent_resolution_prefers_primary_agent() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary Cursor Agent installation");
        let primary_bin = temporary.path().join("primary-bin");
        let primary_version = temporary.path().join("versions/current");
        let legacy_bin = temporary.path().join("legacy-bin");
        let excluded_directory = temporary.path().join("legacy-aliases");
        for directory in [
            &primary_bin,
            &primary_version,
            &legacy_bin,
            &excluded_directory,
        ] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        let primary = primary_version.join("cursor-agent");
        write_executable(&primary, "synthetic primary Cursor Agent");
        symlink(&primary, primary_bin.join("agent")).expect("primary agent alias");
        let legacy = legacy_bin.join("cursor-agent");
        write_executable(&legacy, "synthetic legacy Cursor Agent");
        let current_exe = temporary.path().join("omni");
        write_executable(&current_exe, "#!/bin/sh\nexit 0\n");

        let path = env::join_paths([&legacy_bin, &primary_bin]).expect("fixture PATH");
        let resolved = resolve_cursor_agent_binary(&path, &excluded_directory, &current_exe)
            .expect("prefer primary agent");

        assert_eq!(resolved, primary.canonicalize().expect("primary agent"));
    }

    #[cfg(unix)]
    #[test]
    fn cursor_agent_resolution_retains_legacy_fallback() {
        let temporary = tempfile::tempdir().expect("temporary Cursor Agent installation");
        let bin = temporary.path().join("bin");
        let excluded_directory = temporary.path().join("legacy-aliases");
        fs::create_dir_all(&bin).expect("binary directory");
        fs::create_dir_all(&excluded_directory).expect("excluded directory");
        let cursor_agent = bin.join("cursor-agent");
        write_executable(&cursor_agent, "synthetic legacy Cursor Agent");
        let current_exe = temporary.path().join("omni");
        write_executable(&current_exe, "#!/bin/sh\nexit 0\n");

        let path = env::join_paths([&bin]).expect("fixture PATH");
        let resolved = resolve_cursor_agent_binary(&path, &excluded_directory, &current_exe)
            .expect("resolve legacy Cursor Agent");

        assert_eq!(resolved, cursor_agent.canonicalize().expect("Cursor Agent"));
    }

    #[cfg(unix)]
    #[test]
    fn cursor_agent_resolution_rejects_unrelated_agent() {
        let temporary = tempfile::tempdir().expect("temporary command installation");
        let unrelated_bin = temporary.path().join("unrelated");
        let legacy_bin = temporary.path().join("legacy");
        let excluded_directory = temporary.path().join("legacy-aliases");
        for directory in [&unrelated_bin, &legacy_bin, &excluded_directory] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        write_executable(&unrelated_bin.join("agent"), "synthetic unrelated agent");
        let current_exe = temporary.path().join("omni");
        write_executable(&current_exe, "#!/bin/sh\nexit 0\n");
        let unrelated_path = env::join_paths([&unrelated_bin]).expect("unrelated PATH");

        assert!(
            resolve_cursor_agent_binary(&unrelated_path, &excluded_directory, &current_exe)
                .is_err()
        );

        let cursor_agent = legacy_bin.join("cursor-agent");
        write_executable(&cursor_agent, "synthetic legacy Cursor Agent");
        let fallback_path = env::join_paths([&unrelated_bin, &legacy_bin]).expect("fallback PATH");
        assert_eq!(
            resolve_cursor_agent_binary(&fallback_path, &excluded_directory, &current_exe)
                .expect("skip unrelated agent"),
            cursor_agent.canonicalize().expect("Cursor Agent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cursor_ide_resolution_keeps_older_desktop_out_of_cross_import_targets() {
        let temporary = tempfile::tempdir().expect("temporary Cursor installation");
        let agent_wrapper = temporary.path().join("cursor");
        let agent_marker = temporary.path().join("agent-wrapper-launched");
        write_executable(
            &agent_wrapper,
            &format!("#!/bin/sh\n: >{}\n", shell_quote(&agent_marker)),
        );
        let desktop = temporary.path().join("Cursor-3.12.16-x86_64.AppImage");
        let desktop_marker = temporary.path().join("desktop-launched");
        write_executable(
            &desktop,
            &format!("#!/bin/sh\n: >{}\n", shell_quote(&desktop_marker)),
        );
        let supported = temporary.path().join("Cursor-3.12.17-x86_64.AppImage");
        let supported_marker = temporary.path().join("supported-desktop-launched");
        write_executable(
            &supported,
            &format!("#!/bin/sh\n: >{}\n", shell_quote(&supported_marker)),
        );

        assert!(validate_cursor_ide_binary(&agent_wrapper).is_err());
        let resolved = select_cursor_ide_binary([agent_wrapper, desktop.clone()])
            .expect("resolve older Cursor desktop");

        assert_eq!(resolved, desktop.canonicalize().expect("Cursor desktop"));
        assert!(!cursor_ide_cross_import_ready(&resolved));
        assert!(cursor_ide_cross_import_ready(&supported));
        assert!(!agent_marker.exists());
        assert!(!desktop_marker.exists());
        assert!(!supported_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cursor_ide_resolution_accepts_unversioned_appimage_for_workspace_only() {
        let temporary = tempfile::tempdir().expect("temporary Cursor installation");
        let desktop = temporary.path().join("Cursor.AppImage");
        let launch_marker = temporary.path().join("desktop-launched");
        write_executable(
            &desktop,
            &format!("#!/bin/sh\n: >{}\n", shell_quote(&launch_marker)),
        );

        let resolved = select_cursor_ide_binary([desktop.clone()])
            .expect("resolve unversioned Cursor desktop");

        assert_eq!(resolved, desktop.canonicalize().expect("Cursor desktop"));
        assert!(!cursor_ide_cross_import_ready(&resolved));
        assert!(!launch_marker.exists());
    }

    #[test]
    fn windows_pathext_skips_extensionless_npm_shim() {
        let npm_directory = Path::new(r"C:\Users\developer\AppData\Roaming\npm");
        let candidates =
            windows_executable_candidates(npm_directory, "claude", Some(OsStr::new(".CMD;.EXE")));

        assert_eq!(
            candidates,
            vec![
                npm_directory.join("claude.CMD"),
                npm_directory.join("claude.EXE")
            ]
        );
        assert!(!candidates.contains(&npm_directory.join("claude")));
    }

    #[test]
    fn windows_default_pathext_finds_npm_commands() {
        let candidates = windows_executable_candidates(
            Path::new(r"C:\Users\developer\AppData\Roaming\npm"),
            "claude",
            None,
        );
        assert!(candidates.iter().any(|path| path.ends_with("claude.EXE")));
        assert!(candidates.iter().any(|path| path.ends_with("claude.CMD")));
        assert!(!candidates.iter().any(|path| path.ends_with("claude.ps1")));
    }
}
