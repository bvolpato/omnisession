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

/// Checks file identity without requiring read access to an executable on Unix.
pub(super) fn same_executable(left: &Path, right: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = fs::metadata(left)?;
        let right = fs::metadata(right)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    same_file::is_same_file(left, right)
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct ExecutableRevision {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
impl From<&fs::Metadata> for ExecutableRevision {
    fn from(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[cfg(unix)]
static LAST_COMPARISON: std::sync::Mutex<Option<([ExecutableRevision; 2], bool)>> =
    std::sync::Mutex::new(None);

pub(super) fn is_codex_executable_copy(candidate: &Path, provider: &Path) -> bool {
    compare_codex_executables(candidate, provider).unwrap_or(false)
}

fn compare_codex_executables(candidate: &Path, provider: &Path) -> std::io::Result<bool> {
    let paths = [candidate, provider];
    let metadata = [fs::metadata(candidate)?, fs::metadata(provider)?];
    if metadata[0].len() != metadata[1].len() || metadata.iter().any(|meta| !meta.is_file()) {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.iter().any(|meta| {
            let mode = meta.permissions().mode();
            mode & 0o444 == 0 || mode & 0o111 == 0
        }) {
            return Ok(false);
        }
    }
    #[cfg(windows)]
    if paths.iter().any(|path| {
        !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    }) {
        return Ok(false);
    }

    #[cfg(unix)]
    let revisions = metadata.each_ref().map(ExecutableRevision::from);
    #[cfg(unix)]
    if let Some((known, result)) = *LAST_COMPARISON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        && known == revisions
    {
        return Ok(result);
    }

    let open = |path: &Path| {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        options.open(path)
    };
    let mut files = [open(paths[0])?, open(paths[1])?];
    let size = files[0].metadata()?.len();
    if size != files[1].metadata()?.len() || size != metadata[0].len() {
        return Ok(false);
    }
    let matches = native_executable(&mut files[0])? && equal_executable_contents(&mut files, size)?;
    #[cfg(unix)]
    {
        for ((path, file), before) in paths.iter().zip(&files).zip(revisions) {
            if ExecutableRevision::from(&file.metadata()?) != before
                || ExecutableRevision::from(&fs::metadata(path)?) != before
            {
                return Ok(false);
            }
        }
        *LAST_COMPARISON
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((revisions, matches));
    }
    Ok(matches)
}

fn native_executable(file: &mut fs::File) -> std::io::Result<bool> {
    use std::io::Read;
    let mut header = [0; 64];
    file.read_exact(&mut header)?;
    #[cfg(target_os = "macos")]
    {
        let magic = u32::from_be_bytes(header[..4].try_into().unwrap());
        Ok(match magic {
            0xfeed_face | 0xfeed_facf => header[12..16] == 2u32.to_be_bytes(),
            0xcefa_edfe | 0xcffa_edfe => header[12..16] == 2u32.to_le_bytes(),
            0xcafe_babe | 0xcafe_babf => {
                (1..=64).contains(&u32::from_be_bytes(header[4..8].try_into().unwrap()))
                    && matches!(
                        u32::from_be_bytes(header[8..12].try_into().unwrap()) & 0x00ff_ffff,
                        7 | 12 | 18
                    )
            }
            _ => false,
        })
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        if &header[..2] != b"MZ" {
            return Ok(false);
        }
        let offset = u32::from_le_bytes(header[60..64].try_into().unwrap());
        file.seek(SeekFrom::Start(u64::from(offset)))?;
        let mut signature = [0; 4];
        file.read_exact(&mut signature)?;
        Ok(&signature == b"PE\0\0")
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Ok(&header[..4] == b"\x7fELF"
            && matches!(header[4], 1 | 2)
            && matches!(header[5], 1 | 2)
            && header[6] == 1)
    }
}

fn equal_executable_contents(files: &mut [fs::File; 2], mut size: u64) -> std::io::Result<bool> {
    use std::io::{Read, Seek};
    files[0].rewind()?;
    let mut left = vec![0; 64 * 1024];
    let mut right = vec![0; left.len()];
    while size != 0 {
        let len = usize::try_from(size.min(left.len() as u64)).expect("bounded read");
        files[0].read_exact(&mut left[..len])?;
        files[1].read_exact(&mut right[..len])?;
        if left[..len] != right[..len] {
            return Ok(false);
        }
        size -= len as u64;
    }
    Ok(true)
}

/// An off-PATH Codex installation identifies its executable in the standalone or npm manifest.
pub(super) fn is_codex_package_entrypoint(program: &Path) -> bool {
    use std::io::Read;
    const MANIFEST_LIMIT: u64 = 64 * 1024;
    let Ok(command) = provider_process(program) else {
        return false;
    };
    // A validated Windows npm launcher passes its package script to Node as the first argument.
    let entrypoint = command
        .get_args()
        .next()
        .unwrap_or_else(|| command.get_program());
    let Ok(program) = fs::canonicalize(entrypoint) else {
        return false;
    };
    let Some(bin) = program
        .parent()
        .filter(|directory| directory.ends_with("bin"))
    else {
        return false;
    };
    let Some(root) = bin.parent() else {
        return false;
    };
    let Some(name) = program.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let manifest = match name {
        "codex" | "codex.exe" => root.join("codex-package.json"),
        "codex.js" => root.join("package.json"),
        _ => return false,
    };
    if !manifest
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() <= MANIFEST_LIMIT)
    {
        return false;
    }
    let Some(metadata) = fs::File::open(manifest).ok().and_then(|file| {
        serde_json::from_reader::<_, serde_json::Value>(file.take(MANIFEST_LIMIT)).ok()
    }) else {
        return false;
    };
    if name == "codex.js" {
        metadata["name"] == "@openai/codex" && metadata["bin"]["codex"] == "bin/codex.js"
    } else {
        metadata["layoutVersion"] == 1
            && metadata["variant"] == "codex"
            && metadata["entrypoint"] == format!("bin/{name}")
    }
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

    #[test]
    fn native_copy_comparison_observes_the_last_byte_and_file_replacements() -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let temp = tempfile::tempdir()?;
        let provider = temp.path().join("provider.exe");
        let copy = temp.path().join("candidate.exe");
        #[cfg(unix)]
        fs::copy("/usr/bin/true", &provider)?;
        #[cfg(windows)]
        {
            let source =
                PathBuf::from(env::var_os("SystemRoot").context("Windows system directory")?)
                    .join("System32")
                    .join("where.exe");
            fs::copy(source, &provider)?;
        }
        fs::copy(&provider, &copy)?;
        assert!(is_codex_executable_copy(&copy, &provider));
        assert!(is_codex_executable_copy(&copy, &provider));
        let mut file = fs::OpenOptions::new().read(true).write(true).open(&copy)?;
        let modified = file.metadata()?.modified()?;
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        file.seek(SeekFrom::End(-1))?;
        file.write_all(&[!tail[0]])?;
        file.set_modified(modified)?;
        drop(file);
        assert!(!is_codex_executable_copy(&copy, &provider));
        fs::copy(&provider, &copy)?;
        assert!(is_codex_executable_copy(&copy, &provider));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o111))?;
            assert!(!is_codex_executable_copy(&copy, &provider));
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o700))?;
        }
        fs::write(&copy, b"#!/bin/sh\nexec ./runtime\n")?;
        fs::copy(&copy, &provider)?;
        assert!(!is_codex_executable_copy(&copy, &provider));
        Ok(())
    }

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
