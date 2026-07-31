use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use omnis_store::state_root;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

const RELEASE_BASE_URL: &str = "https://github.com/bvolpato/omnisession/releases/download";
const MAX_CHECKSUM_FILE_SIZE: u64 = 64 * 1024;
const MAX_ARCHIVE_SIZE: u64 = 64 * 1024 * 1024;
const MAX_BINARY_SIZE: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_LIST_SIZE: usize = 4 * 1024;
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

pub fn supported() -> bool {
    artifact_name().is_some()
        && env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(ToOwned::to_owned))
            .is_some_and(|name| name == "omni")
}

pub fn install(version: &str) -> Result<()> {
    validate_version(version)?;
    let artifact = artifact_name().context("this platform has no OmniSession release artifact")?;
    let current_exe = env::current_exe().context("resolving current OmniSession binary")?;
    if current_exe.file_name().is_none_or(|name| name != "omni") {
        bail!("current executable is not named `omni`; use the installer to update");
    }
    let install_dir = current_exe
        .parent()
        .context("current OmniSession binary has no parent directory")?;
    let _lock = acquire_update_lock()?;
    let temporary = tempfile::tempdir().context("creating update workspace")?;
    let archive = temporary.path().join(&artifact);
    let checksums = temporary.path().join("SHA256SUMS");
    let release_url = format!("{RELEASE_BASE_URL}/v{version}");

    println!(
        "Updating OmniSession v{} -> v{version}...",
        env!("CARGO_PKG_VERSION")
    );
    download(
        &format!("{release_url}/{artifact}"),
        &archive,
        MAX_ARCHIVE_SIZE,
    )?;
    download(
        &format!("{release_url}/SHA256SUMS"),
        &checksums,
        MAX_CHECKSUM_FILE_SIZE,
    )?;
    verify_checksum(&archive, &checksums, &artifact)?;
    verify_archive_layout(&archive)?;

    let candidate = temporary.path().join("omni");
    extract_bounded_member(&archive, "omni", &candidate, MAX_BINARY_SIZE)?;
    validate_regular_file(&candidate, "release binary")?;
    publish_binary(&candidate, &current_exe, install_dir, version)?;
    println!("Updated OmniSession to v{version}.");
    Ok(())
}

fn artifact_name() -> Option<String> {
    let platform = match env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let architecture = match env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    Some(format!("omni-{platform}-{architecture}.tar.gz"))
}

fn validate_version(version: &str) -> Result<()> {
    let mut parts = version.split('.');
    let valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && parts.next().is_none();
    if !valid {
        bail!("invalid OmniSession release version `{version}`");
    }
    Ok(())
}

fn acquire_update_lock() -> Result<File> {
    let locks = state_root()?.join("locks");
    if locks.exists() {
        let metadata = fs::symlink_metadata(&locks).context("reading update lock directory")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("OmniSession update lock directory is not a regular directory");
        }
    } else {
        fs::create_dir(&locks).context("creating update lock directory")?;
    }
    let path = locks.join("self-update.lock");
    if path.exists()
        && fs::symlink_metadata(&path)
            .context("reading self-update lock")?
            .file_type()
            .is_symlink()
    {
        bail!("OmniSession self-update lock must not be a symlink");
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .context("opening self-update lock")?;
    lock.try_lock_exclusive()
        .context("another OmniSession update is running")?;
    Ok(lock)
}

fn download(url: &str, destination: &Path, maximum_size: u64) -> Result<()> {
    let filename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("release asset");
    println!("Downloading {filename}...");
    let maximum_size_arg = maximum_size.to_string();
    let status = Command::new("curl")
        .args([
            "--http1.1",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--speed-limit",
            "1024",
            "--speed-time",
            "30",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--retry",
            "4",
            "--retry-all-errors",
            "--max-filesize",
            &maximum_size_arg,
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .status()
        .context("starting curl for OmniSession update")?;
    if !status.success() {
        bail!("could not download `{url}`");
    }
    let size = fs::metadata(destination)
        .context("reading downloaded release asset metadata")?
        .len();
    if size == 0 || size > maximum_size {
        bail!("downloaded release asset has invalid size");
    }
    Ok(())
}

fn verify_checksum(archive: &Path, checksums: &Path, artifact: &str) -> Result<()> {
    let metadata = fs::metadata(checksums).context("reading release checksum metadata")?;
    if metadata.len() > MAX_CHECKSUM_FILE_SIZE {
        bail!("release checksum file exceeds safe size");
    }
    let document = fs::read_to_string(checksums).context("reading release checksums")?;
    let matches = document
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let checksum = fields.next()?;
            let filename = fields.next()?;
            (filename == artifact && fields.next().is_none()).then_some(checksum)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1
        || matches[0].len() != 64
        || !matches[0].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("release checksums do not contain one valid entry for `{artifact}`");
    }
    let actual = sha256_file(archive)?;
    if !actual.eq_ignore_ascii_case(matches[0]) {
        bail!("SHA-256 verification failed for `{artifact}`");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("opening release archive for verification")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("reading release archive for verification")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn verify_archive_layout(archive: &Path) -> Result<()> {
    let output = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .context("listing OmniSession release archive")?;
    if !output.status.success() || output.stdout.len() > MAX_ARCHIVE_LIST_SIZE {
        bail!("could not safely inspect OmniSession release archive");
    }
    let entries = std::str::from_utf8(&output.stdout)
        .context("release archive paths are not UTF-8")?
        .lines()
        .collect::<Vec<_>>();
    if entries.len() != 2 || !entries.contains(&"omni") || !entries.contains(&"LICENSE") {
        bail!("OmniSession release archive has unexpected contents");
    }
    Ok(())
}

fn extract_bounded_member(
    archive: &Path,
    member: &str,
    destination: &Path,
    maximum_size: u64,
) -> Result<()> {
    let mut child = Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg(member)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting bounded release extraction")?;
    let mut stdout = child
        .stdout
        .take()
        .context("capturing bounded release extraction")?;
    let mut output = File::create(destination).context("creating extracted release binary")?;
    let extracted = (|| -> Result<()> {
        let mut total = 0_u64;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = stdout
                .read(&mut buffer)
                .context("reading extracted release binary")?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > maximum_size {
                bail!("extracted release binary exceeds safe size");
            }
            output
                .write_all(&buffer[..read])
                .context("writing extracted release binary")?;
        }
        output.flush().context("flushing extracted release binary")
    })();
    if let Err(error) = extracted {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child
        .wait()
        .context("waiting for bounded release extraction")?;
    if !status.success() {
        bail!("could not extract OmniSession release binary");
    }
    Ok(())
}

fn validate_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("reading {label}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{label} is not a regular file");
    }
    Ok(())
}

fn verify_binary_version(binary: &Path, version: &str) -> Result<()> {
    let mut output = tempfile::tempfile().context("creating version output buffer")?;
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            output
                .try_clone()
                .context("cloning version output buffer")?,
        ))
        .stderr(Stdio::null())
        .spawn()
        .context("starting downloaded OmniSession binary")?;
    let status = child
        .wait_timeout(VERSION_CHECK_TIMEOUT)
        .context("waiting for downloaded OmniSession binary")?;
    let Some(status) = status else {
        child
            .kill()
            .context("stopping downloaded OmniSession binary")?;
        child
            .wait()
            .context("reaping downloaded OmniSession binary")?;
        bail!("downloaded OmniSession binary version check timed out");
    };
    if output
        .metadata()
        .context("reading version output metadata")?
        .len()
        > 1024
    {
        bail!("downloaded OmniSession binary version output exceeds safe size");
    }
    output.rewind().context("rewinding version output buffer")?;
    let mut reported = String::new();
    output
        .read_to_string(&mut reported)
        .context("reading downloaded OmniSession version")?;
    let expected = format!("omni {version}");
    if !status.success() || reported.trim() != expected {
        bail!("downloaded binary did not report `{expected}`");
    }
    Ok(())
}

fn publish_binary(candidate: &Path, current: &Path, directory: &Path, version: &str) -> Result<()> {
    let mut staged = tempfile::NamedTempFile::new_in(directory)
        .context("creating same-directory update file")?;
    copy_and_sync(candidate, &mut staged)?;
    let staged = staged.into_temp_path();
    verify_binary_version(&staged, version)?;
    let mut backup = tempfile::NamedTempFile::new_in(directory)
        .context("creating same-directory update backup")?;
    copy_and_sync(current, &mut backup)?;

    staged
        .persist(current)
        .map_err(|error| error.error)
        .context("atomically replacing OmniSession binary")?;
    let published =
        sync_directory(directory).and_then(|()| verify_binary_version(current, version));
    if let Err(error) = published {
        let rollback = backup
            .persist(current)
            .map_err(|persist_error| persist_error.error)
            .context("restoring previous OmniSession binary")
            .and_then(|_| sync_directory(directory));
        return match rollback {
            Ok(()) => Err(error).context("updated binary failed verification and was rolled back"),
            Err(rollback_error) => Err(error).context(format!(
                "updated binary failed verification and rollback also failed: {rollback_error:#}"
            )),
        };
    }
    Ok(())
}

fn copy_and_sync(source: &Path, destination: &mut tempfile::NamedTempFile) -> Result<()> {
    let mut input = File::open(source).context("opening update source binary")?;
    std::io::copy(&mut input, destination.as_file_mut()).context("copying update binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        destination
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .context("setting update binary permissions")?;
    }
    destination.flush().context("flushing update binary")?;
    destination
        .as_file()
        .sync_all()
        .context("syncing update binary")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .context("opening install directory for sync")?
        .sync_all()
        .context("syncing install directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_require_three_numeric_components() {
        assert!(validate_version("0.8.36").is_ok());
        assert!(validate_version("v0.8.36").is_err());
        assert!(validate_version("0.8").is_err());
        assert!(validate_version("0.8.36-rc1").is_err());
        assert!(validate_version("0.8.36/asset").is_err());
    }

    #[test]
    fn checksum_verification_rejects_wrong_digest() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let archive = directory.path().join("omni-linux-x86_64.tar.gz");
        let checksums = directory.path().join("SHA256SUMS");
        fs::write(&archive, b"release").expect("archive fixture");
        fs::write(
            &checksums,
            format!("{}  omni-linux-x86_64.tar.gz\n", "0".repeat(64)),
        )
        .expect("checksum fixture");
        assert!(
            verify_checksum(&archive, &checksums, "omni-linux-x86_64.tar.gz")
                .expect_err("wrong digest")
                .to_string()
                .contains("SHA-256 verification failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn publish_replaces_binary_after_version_check() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary install directory");
        let current = directory.path().join("omni");
        let candidate = directory.path().join("candidate");
        fs::write(&current, "#!/bin/sh\nprintf 'omni 0.8.35\\n'\n").expect("old binary");
        fs::write(&candidate, "#!/bin/sh\nprintf 'omni 0.8.36\\n'\n").expect("new binary");
        fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).expect("old permissions");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))
            .expect("new permissions");

        publish_binary(&candidate, &current, directory.path(), "0.8.36").expect("publish update");
        verify_binary_version(&current, "0.8.36").expect("updated binary version");
    }
}
