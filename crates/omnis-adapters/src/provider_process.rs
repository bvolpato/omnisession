//! Validated provider subprocess preparation shared by the CLI and adapters.

use std::{path::Path, process::Command};

/// Starts a Windows console helper in its own process group on a hidden console.
/// Terminal Ctrl+C and Ctrl+Break then reach the interactive parent without interrupting
/// the helper during a native write or rollback.
#[cfg(windows)]
pub const WINDOWS_HELPER_CREATION_FLAGS: u32 = {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
};

mod bounded_output;
pub use bounded_output::{
    ProviderProbe, bounded_provider_help_output, bounded_provider_json_output,
    bounded_provider_line_output, bounded_provider_output, bounded_provider_text_output,
    inherit_provider_probes, with_provider_probes,
};

#[cfg(unix)]
pub(crate) use bounded_output::bounded_terminal_output;

#[cfg(any(test, windows))]
use std::path::PathBuf;
#[cfg(windows)]
use std::{env, ffi::OsString, fs};

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(test, windows))]
use anyhow::bail;

/// Prepares a provider process that never runs through `cmd.exe`; callers add arguments.
///
/// Windows runs validated npm command shims through `node.exe` and refuses other batch files.
/// Other programs, and every program on other platforms, run directly.
///
/// # Errors
///
/// Returns an error when a Windows batch launcher does not match the npm shim contract or its
/// Node runtime or target cannot be validated.
#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
pub fn provider_process(program: &Path) -> Result<Command> {
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
    if !crate::support::is_batch_launcher(program) {
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

#[cfg(windows)]
fn legacy_alias_directory() -> Result<PathBuf> {
    crate::support::legacy_alias_directory().context("resolving OmniSession legacy aliases")
}

#[cfg(windows)]
fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(windows)]
fn same_path(left: &Path, right: &Path) -> bool {
    canonical_or_original(left) == canonical_or_original(right)
}

#[cfg(windows)]
fn validate_real_binary(
    candidate: &Path,
    excluded_directory: &Path,
    current_exe: &Path,
) -> Result<PathBuf> {
    if !candidate.is_file() {
        bail!("`{}` is not executable", candidate.display());
    }
    let candidate = fs::canonicalize(candidate)
        .with_context(|| format!("canonicalizing `{}`", candidate.display()))?;
    if candidate == current_exe
        || same_file::is_same_file(&candidate, current_exe).unwrap_or(false)
        || candidate.starts_with(canonical_or_original(excluded_directory))
    {
        bail!(
            "`{}` resolves to an OmniSession executable or legacy alias",
            candidate.display()
        );
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::fs;

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
