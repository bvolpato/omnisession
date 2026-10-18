//! Provider executable discovery and subprocess preparation.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(windows)]
use std::ffi::OsString;

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

/// Prepares a provider process that never runs through `cmd.exe`; callers add arguments.
///
/// Windows runs validated npm command shims through `node.exe` and refuses other batch files.
/// Other programs, and every program on other platforms, run directly.
#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
pub(super) fn provider_process(program: &Path) -> Result<Command> {
    #[cfg(windows)]
    {
        windows_provider_command(program)
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new(program))
    }
}

#[cfg(windows)]
fn windows_provider_command(program: &Path) -> Result<Command> {
    if !program
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    {
        return Ok(Command::new(program));
    }

    let script = resolve_npm_cmd_shim_target(program).with_context(|| {
        format!(
            "refusing to execute unrecognized batch provider `{}` directly",
            program.display()
        )
    })?;
    let shim_parent = program
        .canonicalize()
        .with_context(|| format!("canonicalizing npm command shim `{}`", program.display()))?
        .parent()
        .context("npm command shim has no parent directory")?
        .to_path_buf();
    let node = resolve_node_executable(&shim_parent)?;
    let node = windows_application_path(&node).context("preparing node.exe application path")?;
    let script =
        windows_application_path(&script).context("preparing npm target application path")?;
    let mut command = Command::new(node);
    command.arg(script);
    Ok(command)
}

#[cfg(windows)]
fn windows_application_path(path: &Path) -> Result<PathBuf> {
    let ordinary = windows_ordinary_path(path)?;
    match same_file::is_same_file(path, &ordinary) {
        Ok(true) => Ok(ordinary),
        Ok(false) => bail!(
            "ordinary Windows application path `{}` does not identify `{}`",
            ordinary.display(),
            path.display()
        ),
        Err(error) => Err(error).with_context(|| {
            format!(
                "verifying Windows application path `{}` against `{}`",
                ordinary.display(),
                path.display()
            )
        }),
    }
}

#[cfg(windows)]
fn windows_ordinary_path(path: &Path) -> Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let verbatim_prefix = [
        u16::from(b'\\'),
        u16::from(b'\\'),
        u16::from(b'?'),
        u16::from(b'\\'),
    ];
    let unc_prefix = [
        u16::from(b'U'),
        u16::from(b'N'),
        u16::from(b'C'),
        u16::from(b'\\'),
    ];
    let ordinary = if let Some(remainder) = path.strip_prefix(&verbatim_prefix) {
        if remainder.starts_with(&unc_prefix) {
            let mut ordinary = vec![u16::from(b'\\'), u16::from(b'\\')];
            ordinary.extend_from_slice(&remainder[unc_prefix.len()..]);
            ordinary
        } else if is_windows_drive_absolute(remainder) {
            remainder.to_vec()
        } else {
            bail!(
                "unsupported verbatim Windows application path `{}`",
                PathBuf::from(OsString::from_wide(&path)).display()
            );
        }
    } else {
        path
    };
    if !is_windows_drive_absolute(&ordinary) && !is_windows_unc_absolute(&ordinary) {
        bail!(
            "Windows application path must be an absolute drive or UNC path: `{}`",
            PathBuf::from(OsString::from_wide(&ordinary)).display()
        );
    }
    Ok(PathBuf::from(OsString::from_wide(&ordinary)))
}

#[cfg(windows)]
fn is_windows_drive_absolute(path: &[u16]) -> bool {
    path.len() >= 3
        && ((u16::from(b'A')..=u16::from(b'Z')).contains(&path[0])
            || (u16::from(b'a')..=u16::from(b'z')).contains(&path[0]))
        && path[1] == u16::from(b':')
        && path[2] == u16::from(b'\\')
}

#[cfg(windows)]
fn is_windows_unc_absolute(path: &[u16]) -> bool {
    let separator = u16::from(b'\\');
    if !path.starts_with(&[separator, separator]) {
        return false;
    }
    let mut components = path[2..].split(|unit| *unit == separator);
    let server = components.next().unwrap_or_default();
    let share = components.next().unwrap_or_default();
    !server.is_empty()
        && server != [u16::from(b'.')]
        && server != [u16::from(b'?')]
        && !share.is_empty()
}

#[cfg(windows)]
fn resolve_npm_cmd_shim_target(shim: &Path) -> Result<PathBuf> {
    use std::io::Read;

    const MAX_NPM_CMD_SHIM_BYTES: u64 = 64 * 1024;

    let mut contents = Vec::new();
    fs::File::open(shim)
        .with_context(|| format!("opening npm command shim `{}`", shim.display()))?
        .take(MAX_NPM_CMD_SHIM_BYTES + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("reading npm command shim `{}`", shim.display()))?;
    if contents.len() as u64 > MAX_NPM_CMD_SHIM_BYTES {
        bail!(
            "npm command shim `{}` exceeds {MAX_NPM_CMD_SHIM_BYTES} bytes",
            shim.display()
        );
    }
    let contents = std::str::from_utf8(&contents)
        .with_context(|| format!("npm command shim `{}` is not UTF-8", shim.display()))?;
    let relative_target = npm_cmd_shim_relative_target(contents)?;
    let shim = shim
        .canonicalize()
        .with_context(|| format!("canonicalizing npm command shim `{}`", shim.display()))?;
    let parent = shim
        .parent()
        .context("npm command shim has no parent directory")?
        .canonicalize()
        .context("canonicalizing npm command shim directory")?;
    let target = parent
        .join(relative_target)
        .canonicalize()
        .with_context(|| {
            format!(
                "canonicalizing npm command shim target from `{}`",
                shim.display()
            )
        })?;
    if target == parent || !target.starts_with(&parent) || !target.is_file() {
        bail!(
            "npm command shim target `{}` is not a file within `{}`",
            target.display(),
            parent.display()
        );
    }
    require_node_shebang(&target)?;
    Ok(target)
}

#[cfg(any(test, windows))]
fn npm_cmd_shim_relative_target(contents: &str) -> Result<PathBuf> {
    let lines = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if let Some(target) = current_npm_cmd_shim_target(&lines) {
        return Ok(PathBuf::from(target));
    }
    if let Some(target) = legacy_npm_cmd_shim_target(&lines) {
        return Ok(PathBuf::from(target));
    }
    bail!("batch file does not match npm Node command shim contract")
}

#[cfg(any(test, windows))]
fn current_npm_cmd_shim_target<'a>(lines: &[&'a str]) -> Option<&'a str> {
    const HEAD: [&str; 12] = [
        "@ECHO off",
        "GOTO start",
        ":find_dp0",
        "SET dp0=%~dp0",
        "EXIT /b",
        ":start",
        "SETLOCAL",
        "CALL :find_dp0",
        "IF EXIST \"%dp0%\\node.exe\" (",
        "SET \"_prog=%dp0%\\node.exe\"",
        ") ELSE (",
        "SET \"_prog=node\"",
    ];
    const LATEST_LAUNCH_PREFIX: &str = "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"";
    const PREVIOUS_LAUNCH_PREFIX: &str =
        "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"";
    const PATHEXT: &str = "SET PATHEXT=%PATHEXT:;.JS;=;%";

    if lines.len() <= HEAD.len() || !batch_lines_equal(&lines[..HEAD.len()], &HEAD) {
        return None;
    }
    match &lines[HEAD.len()..] {
        [close, launch] if close.eq_ignore_ascii_case(")") => {
            cmd_shim_launch_target(launch, LATEST_LAUNCH_PREFIX)
        }
        [pathext, close, launch]
            if pathext.eq_ignore_ascii_case(PATHEXT) && close.eq_ignore_ascii_case(")") =>
        {
            cmd_shim_launch_target(launch, PREVIOUS_LAUNCH_PREFIX)
        }
        _ => None,
    }
}

#[cfg(any(test, windows))]
fn legacy_npm_cmd_shim_target<'a>(lines: &[&'a str]) -> Option<&'a str> {
    const BEFORE_LAUNCH: [&str; 9] = [
        "@ECHO off",
        "SETLOCAL",
        "CALL :find_dp0",
        "IF EXIST \"%dp0%\\node.exe\" (",
        "SET \"_prog=%dp0%\\node.exe\"",
        ") ELSE (",
        "SET \"_prog=node\"",
        "SET PATHEXT=%PATHEXT:;.JS;=;%",
        ")",
    ];
    const AFTER_LAUNCH: [&str; 5] = [
        "ENDLOCAL",
        "EXIT /b %errorlevel%",
        ":find_dp0",
        "SET dp0=%~dp0",
        "EXIT /b",
    ];

    let launch_index = BEFORE_LAUNCH.len();
    if lines.len() != BEFORE_LAUNCH.len() + 1 + AFTER_LAUNCH.len()
        || !batch_lines_equal(&lines[..launch_index], &BEFORE_LAUNCH)
        || !batch_lines_equal(&lines[launch_index + 1..], &AFTER_LAUNCH)
    {
        return None;
    }
    cmd_shim_launch_target(lines[launch_index], "\"%_prog%\"")
}

#[cfg(any(test, windows))]
fn batch_lines_equal(actual: &[&str], expected: &[&str]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

#[cfg(any(test, windows))]
fn cmd_shim_launch_target<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let (candidate_prefix, remainder) = line.split_at_checked(prefix.len())?;
    if !candidate_prefix.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let remainder = remainder.trim_start().strip_prefix('"')?;
    let target_end = remainder.find('"')?;
    let (target, suffix) = remainder.split_at(target_end);
    if suffix[1..].trim() != "%*" {
        return None;
    }
    strip_cmd_shim_directory(target)
}

#[cfg(any(test, windows))]
fn strip_cmd_shim_directory(value: &str) -> Option<&str> {
    for prefix in ["%dp0%", "%~dp0"] {
        let (candidate, relative) = value.split_at_checked(prefix.len())?;
        if candidate.eq_ignore_ascii_case(prefix)
            && relative.starts_with(['\\', '/'])
            && !relative.contains('%')
        {
            return Some(relative.trim_start_matches(['\\', '/']));
        }
    }
    None
}

#[cfg(windows)]
fn require_node_shebang(script: &Path) -> Result<()> {
    use std::io::Read;

    const MAX_SHEBANG_BYTES: u64 = 1024;

    let mut prefix = Vec::new();
    fs::File::open(script)
        .with_context(|| format!("opening npm target `{}`", script.display()))?
        .take(MAX_SHEBANG_BYTES)
        .read_to_end(&mut prefix)
        .with_context(|| format!("reading npm target `{}`", script.display()))?;
    let first_line = prefix
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .context("npm target does not have a UTF-8 shebang")?;
    if !is_exact_node_shebang(first_line) {
        bail!(
            "npm target `{}` does not have an exact no-argument Node shebang",
            script.display()
        );
    }
    Ok(())
}

#[cfg(any(test, windows))]
fn is_exact_node_shebang(first_line: &str) -> bool {
    let Some(shebang) = first_line.trim_end().strip_prefix("#!").map(str::trim) else {
        return false;
    };
    let tokens = shebang.split_ascii_whitespace().collect::<Vec<_>>();
    match tokens.as_slice() {
        [executable] => executable.rsplit(['/', '\\']).next().is_some_and(|name| {
            name.eq_ignore_ascii_case("node") || name.eq_ignore_ascii_case("node.exe")
        }),
        [environment, executable] => {
            environment.rsplit(['/', '\\']).next().is_some_and(|name| {
                name.eq_ignore_ascii_case("env") || name.eq_ignore_ascii_case("env.exe")
            }) && (executable.eq_ignore_ascii_case("node")
                || executable.eq_ignore_ascii_case("node.exe"))
        }
        _ => false,
    }
}

#[cfg(windows)]
fn resolve_node_executable(shim_parent: &Path) -> Result<PathBuf> {
    let excluded_directory = legacy_alias_directory()?;
    let current_exe = env::current_exe()
        .context("resolving current OmniSession executable")?
        .canonicalize()
        .context("canonicalizing current OmniSession executable")?;
    let adjacent = shim_parent.join("node.exe");
    if let Ok(node) = validate_real_binary(&adjacent, &excluded_directory, &current_exe) {
        return Ok(node);
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            if same_path(&directory, &excluded_directory) {
                continue;
            }
            let candidate = directory.join("node.exe");
            if let Ok(node) = validate_real_binary(&candidate, &excluded_directory, &current_exe) {
                return Ok(node);
            }
        }
    }
    bail!(
        "real `node.exe` not found for npm command shim in `{}`",
        shim_parent.display()
    )
}

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

    #[test]
    fn npm_cmd_shim_parser_accepts_supported_dp0_forms() {
        assert_eq!(
            npm_cmd_shim_relative_target(
                "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"  \"%dp0%\\node_modules\\provider\\cli.js\" %*\r\n"
            )
            .expect("modern npm command shim"),
            PathBuf::from(r"node_modules\provider\cli.js")
        );
        assert_eq!(
            npm_cmd_shim_relative_target(
                "@ECHO off\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\n\"%_prog%\"  \"%dp0%\\node_modules\\provider\\cli.js\" %*\r\nENDLOCAL\r\nEXIT /b %errorlevel%\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n"
            )
            .expect("legacy npm command shim"),
            PathBuf::from(r"node_modules\provider\cli.js")
        );
        assert_eq!(
            npm_cmd_shim_relative_target(
                "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%~dp0\\node_modules\\provider\\cli.js\" %*\r\n"
            )
            .expect("previous npm command shim"),
            PathBuf::from(r"node_modules\provider\cli.js")
        );
    }

    #[test]
    fn npm_cmd_shim_parser_rejects_shell_commands() {
        assert!(npm_cmd_shim_relative_target("@echo provider %*").is_err());
        assert!(npm_cmd_shim_relative_target(r#"node "C:\outside\provider.js" %*"#).is_err());
        assert!(
            npm_cmd_shim_relative_target(r#"node "%dp0%\%MALICIOUS%\provider.js" %*"#).is_err()
        );
        assert!(
            npm_cmd_shim_relative_target(
                "@ECHO off\r\nSET PROVIDER_MODE=unsafe\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\" \"%dp0%\\provider.js\" %*\r\n"
            )
            .is_err()
        );
    }

    #[test]
    fn node_shebang_requires_exact_no_argument_form() {
        assert!(is_exact_node_shebang("#!/usr/bin/env node\r"));
        assert!(is_exact_node_shebang("#!/usr/bin/node"));
        assert!(!is_exact_node_shebang(
            "#!/usr/bin/env -S node --require setup.js"
        ));
        assert!(!is_exact_node_shebang("#!/usr/bin/env MODE=unsafe node"));
        assert!(!is_exact_node_shebang("#!/usr/bin/node --require setup.js"));
    }

    #[cfg(windows)]
    #[test]
    fn application_paths_accept_only_drive_and_unc_verbatim_prefixes() {
        assert_eq!(
            windows_ordinary_path(Path::new(r"\\?\C:\Users\developer\provider.js"))
                .expect("verbatim drive path"),
            PathBuf::from(r"C:\Users\developer\provider.js")
        );
        assert_eq!(
            windows_ordinary_path(Path::new(r"\\?\UNC\server\share\provider.js"))
                .expect("verbatim UNC path"),
            PathBuf::from(r"\\server\share\provider.js")
        );
        assert!(
            windows_ordinary_path(Path::new(
                r"\\?\Volume{00000000-0000-0000-0000-000000000000}\provider.js"
            ))
            .is_err()
        );
        assert!(windows_ordinary_path(Path::new(r"provider.js")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn application_path_conversion_preserves_file_identity() {
        let temporary = tempfile::tempdir().expect("temporary application directory");
        let file = temporary.path().join("provider.js");
        fs::write(&file, "provider").expect("application file");
        let canonical = file.canonicalize().expect("canonical application file");

        let ordinary = windows_application_path(&canonical).expect("ordinary application path");

        assert!(same_file::is_same_file(&canonical, ordinary).expect("same application file"));
        let ambiguous = canonical.with_file_name("provider.js.");
        assert!(windows_application_path(&ambiguous).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn npm_batch_provider_arguments_bypass_cmd_interpretation() {
        let temporary = tempfile::tempdir().expect("temporary npm provider");
        let package = temporary.path().join("node_modules/provider");
        fs::create_dir_all(&package).expect("provider package");
        let script = package.join("cli.js");
        fs::write(
            &script,
            "#!/usr/bin/env node\nprocess.stdout.write(JSON.stringify(process.argv.slice(2)));\n",
        )
        .expect("provider script");
        let provider = temporary.path().join("provider.cmd");
        fs::write(
            &provider,
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"  \"%dp0%\\node_modules\\provider\\cli.js\" %*\r\n",
        )
        .expect("npm command shim");
        let arguments = [
            "spaces & pipe | caret ^ percent % bang !",
            "double \" quote and trailing \\",
            "Unicode 東京 🦀",
        ];

        let mut command = match windows_provider_command(&provider) {
            Ok(command) => command,
            Err(error) if format!("{error:#}").contains("real `node.exe` not found") => return,
            Err(error) => panic!("prepare npm provider: {error:#}"),
        };
        let output = command
            .args(arguments)
            .output()
            .expect("run npm provider through node.exe");

        assert!(
            output.status.success(),
            "node provider exited with {}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let captured: Vec<String> =
            serde_json::from_slice(&output.stdout).expect("provider arguments as JSON");
        assert_eq!(captured, arguments);
    }

    #[cfg(windows)]
    #[test]
    fn unrecognized_batch_provider_fails_closed() {
        let temporary = tempfile::tempdir().expect("temporary batch provider");
        let provider = temporary.path().join("provider.cmd");
        fs::write(&provider, "@echo off\r\necho unsafe %*\r\n").expect("batch provider");

        let error = windows_provider_command(&provider).expect_err("reject batch provider");

        assert!(format!("{error:#}").contains("refusing to execute unrecognized batch provider"));
    }
}
