use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(crate) struct PrivateStoreGuard {
    _file: fs::File,
}

pub(crate) fn acquire(
    provider_root: &Path,
    namespace: &str,
    provider: &str,
    configured_lock_root: Option<&Path>,
) -> Result<PrivateStoreGuard> {
    let canonical_root = fs::canonicalize(provider_root)
        .with_context(|| format!("canonicalizing {provider} provider root"))?;
    let owner = directory_owner(&canonical_root, provider)?;
    let lock_root = match configured_lock_root {
        Some(path) => normalize_absolute_path(path, provider)?,
        None => user_global_lock_root(owner, namespace, provider)?,
    };
    if lock_root.starts_with(&canonical_root) {
        bail!("OmniSession {provider} lock directory must be outside provider storage");
    }
    ensure_private_lock_directory(&lock_root, owner, provider)?;
    let lock_path = lock_root.join(lock_filename(&canonical_root));
    reject_lock_symlink(&lock_path, owner, provider)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {provider} provider lock"))?;
    validate_open_lock_file(&lock_path, &file, owner, provider)?;
    file.lock_exclusive()
        .with_context(|| format!("locking {provider} provider root"))?;
    Ok(PrivateStoreGuard { _file: file })
}

fn normalize_absolute_path(path: &Path, provider: &str) -> Result<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        bail!("{provider} lock root must be absolute");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("{provider} lock root escapes filesystem root");
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

#[cfg(unix)]
fn directory_owner(path: &Path, provider: &str) -> Result<Option<u32>> {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        fs::metadata(path).with_context(|| format!("reading {provider} provider root owner"))?;
    Ok(Some(metadata.uid()))
}

#[cfg(not(unix))]
fn directory_owner(path: &Path, provider: &str) -> Result<Option<u32>> {
    fs::metadata(path).with_context(|| format!("reading {provider} provider root"))?;
    Ok(None)
}

#[cfg(target_os = "linux")]
fn user_global_lock_root(owner: Option<u32>, namespace: &str, provider: &str) -> Result<PathBuf> {
    let owner = owner.with_context(|| format!("{provider} provider root owner is unavailable"))?;
    Ok(PathBuf::from(format!(
        "/tmp/omnisession-{owner}/{namespace}"
    )))
}

#[cfg(target_os = "macos")]
fn user_global_lock_root(owner: Option<u32>, namespace: &str, provider: &str) -> Result<PathBuf> {
    let owner = owner.with_context(|| format!("{provider} provider root owner is unavailable"))?;
    Ok(PathBuf::from(format!(
        "/private/tmp/omnisession-{owner}/{namespace}"
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn user_global_lock_root(_owner: Option<u32>, _namespace: &str, provider: &str) -> Result<PathBuf> {
    bail!("native {provider} provider locking is supported only on Linux and macOS")
}

fn ensure_private_lock_directory(path: &Path, owner: Option<u32>, provider: &str) -> Result<()> {
    let namespace = path
        .parent()
        .with_context(|| format!("{provider} lock directory has no private namespace"))?;
    ensure_owned_private_directory(namespace, owner, provider)?;
    ensure_owned_private_directory(path, owner, provider)
}

fn ensure_owned_private_directory(path: &Path, owner: Option<u32>, provider: &str) -> Result<()> {
    ensure_directory(path)
        .with_context(|| format!("creating OmniSession {provider} lock directory"))?;
    validate_directory_chain(path, provider)?;
    #[cfg(not(unix))]
    let _ = owner;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let expected_owner =
            owner.with_context(|| format!("{provider} provider root owner is unavailable"))?;
        if fs::metadata(path)
            .with_context(|| format!("reading OmniSession {provider} lock directory owner"))?
            .uid()
            != expected_owner
        {
            bail!("OmniSession {provider} lock directory has an unexpected owner");
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!("setting OmniSession {provider} lock directory permissions")
        })?;
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("reading `{}`", path.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("`{}` is not a safe directory", path.display());
        }
        return Ok(());
    }
    let parent = path.parent().context("lock directory has no parent")?;
    ensure_directory(parent)?;
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure_directory(path),
        Err(error) => Err(error).with_context(|| format!("creating `{}`", path.display())),
    }
}

fn validate_directory_chain(path: &Path, provider: &str) -> Result<()> {
    for directory in path.ancestors() {
        if directory.as_os_str().is_empty() {
            break;
        }
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("reading `{}`", directory.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "refusing to lock {provider} provider store through unsafe directory `{}`",
                directory.display()
            );
        }
    }
    Ok(())
}

fn reject_lock_symlink(path: &Path, owner: Option<u32>, provider: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("OmniSession {provider} lock must be a regular file")
        }
        Ok(metadata) => validate_lock_metadata(&metadata, owner, provider),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading OmniSession {provider} lock")),
    }
}

fn validate_open_lock_file(
    path: &Path,
    file: &fs::File,
    owner: Option<u32>,
    provider: &str,
) -> Result<()> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading OmniSession {provider} lock"))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        bail!("OmniSession {provider} lock must be a regular file");
    }
    validate_lock_metadata(&path_metadata, owner, provider)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let file_metadata = file
            .metadata()
            .with_context(|| format!("reading opened OmniSession {provider} lock"))?;
        if (file_metadata.dev(), file_metadata.ino()) != (path_metadata.dev(), path_metadata.ino())
        {
            bail!("OmniSession {provider} lock changed while opening");
        }
        validate_lock_metadata(&file_metadata, owner, provider)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting OmniSession {provider} lock permissions"))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(unix)]
fn validate_lock_metadata(
    metadata: &fs::Metadata,
    owner: Option<u32>,
    provider: &str,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.nlink() != 1 {
        bail!("OmniSession {provider} lock must have exactly one link");
    }
    if metadata.uid()
        != owner.with_context(|| format!("{provider} provider root owner is unavailable"))?
    {
        bail!("OmniSession {provider} lock has an unexpected owner");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_lock_metadata(
    metadata: &fs::Metadata,
    _owner: Option<u32>,
    provider: &str,
) -> Result<()> {
    if !metadata.is_file() {
        bail!("OmniSession {provider} lock must be a regular file");
    }
    Ok(())
}

fn lock_filename(canonical_root: &Path) -> String {
    let mut hash = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        hash.update(canonical_root.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        for unit in canonical_root.as_os_str().encode_wide() {
            hash.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hash.update(canonical_root.as_os_str().to_string_lossy().as_bytes());
    format!("{}.lock", hex::encode(hash.finalize()))
}

#[cfg(all(test, unix))]
pub(crate) fn configured_lock_path(lock_root: &Path, provider_root: &Path) -> Result<PathBuf> {
    Ok(lock_root.join(lock_filename(&fs::canonicalize(provider_root)?)))
}

#[cfg(all(test, unix))]
pub(crate) fn global_lock_path(
    provider_root: &Path,
    namespace: &str,
    provider: &str,
) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(provider_root)?;
    let owner = directory_owner(&canonical_root, provider)?;
    Ok(user_global_lock_root(owner, namespace, provider)?.join(lock_filename(&canonical_root)))
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        time::{Duration, Instant},
    };

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::{process::Command, sync::mpsc};

    use super::*;

    #[test]
    fn provider_root_lock_subprocess() {
        let Some(provider_root) = env::var_os("OMNI_TEST_PRIVATE_LOCK_ROOT") else {
            return;
        };
        let ready = env::var_os("OMNI_TEST_PRIVATE_LOCK_READY")
            .map(PathBuf::from)
            .expect("subprocess ready path");
        let release = env::var_os("OMNI_TEST_PRIVATE_LOCK_RELEASE")
            .map(PathBuf::from)
            .expect("subprocess release path");
        let _guard = acquire(Path::new(&provider_root), "cursor-ide", "Cursor IDE", None)
            .expect("hold provider root lock");
        fs::write(&ready, b"ready").expect("signal held provider root lock");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !release.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting to release provider root lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn user_global_lock_blocks_across_processes_and_state_homes() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let provider_root = temporary.path().join("provider");
        fs::create_dir(&provider_root).expect("provider root");
        let ready = temporary.path().join("lock-ready");
        let release = temporary.path().join("lock-release");
        let other_state_home = PathBuf::from(format!(
            ".omnisession-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        assert!(!other_state_home.exists());
        let mut lock_holder = Command::new(env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "private_store_lock::tests::provider_root_lock_subprocess",
                "--nocapture",
            ])
            .env("OMNI_TEST_PRIVATE_LOCK_ROOT", &provider_root)
            .env("OMNI_TEST_PRIVATE_LOCK_READY", &ready)
            .env("OMNI_TEST_PRIVATE_LOCK_RELEASE", &release)
            .env("OMNISESSION_HOME", &other_state_home)
            .spawn()
            .expect("spawn provider root lock holder");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                lock_holder
                    .try_wait()
                    .expect("inspect provider root lock holder")
                    .is_none(),
                "provider root lock holder exited before acquiring lock"
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for provider root lock holder"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let provider_root_for_thread = provider_root.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(acquire(
                    &provider_root_for_thread,
                    "cursor-ide",
                    "Cursor IDE",
                    None,
                ))
                .expect("report lock acquisition");
        });
        let initial_result = receiver.recv_timeout(Duration::from_millis(100));
        fs::write(&release, b"release").expect("release provider root lock");
        assert!(
            lock_holder
                .wait()
                .expect("wait for provider root lock holder")
                .success(),
            "provider root lock holder failed"
        );
        match initial_result {
            Err(mpsc::RecvTimeoutError::Timeout) => drop(
                receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("lock acquisition unblocked")
                    .expect("acquire provider root lock"),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("lock result channel disconnected")
            }
            Ok(Err(error)) => panic!("lock acquisition failed before release: {error:#}"),
            Ok(Ok(_guard)) => panic!("provider root lock did not block"),
        }
        assert!(!provider_root.join(".omnisession.lock").exists());
        assert!(!other_state_home.exists());

        let canonical_root = fs::canonicalize(&provider_root).expect("canonical provider root");
        let owner = directory_owner(&canonical_root, "Cursor IDE").expect("provider root owner");
        let global_lock_root = user_global_lock_root(owner, "cursor-ide", "Cursor IDE")
            .expect("user-global lock root");
        fs::remove_file(global_lock_root.join(lock_filename(&canonical_root)))
            .expect("remove test lock file");
    }

    #[cfg(unix)]
    #[test]
    fn lock_is_private_and_rejects_unsafe_paths() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};

        let temporary = tempfile::tempdir().expect("temporary root");
        let temporary_root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
        let provider_root = temporary_root.join("provider");
        let lock_root = temporary_root.join("state/locks/cursor-ide");
        fs::create_dir(&provider_root).expect("provider root");
        let provider_lock_root = provider_root.join("locks");
        assert!(
            acquire(
                &provider_root,
                "cursor-ide",
                "Cursor IDE",
                Some(&provider_lock_root),
            )
            .is_err()
        );
        assert!(!provider_lock_root.exists());
        let parent_component_lock_root = provider_root.join("outside/../locks");
        assert!(
            acquire(
                &provider_root,
                "cursor-ide",
                "Cursor IDE",
                Some(&parent_component_lock_root),
            )
            .is_err()
        );
        assert!(!provider_lock_root.exists());

        let guard = acquire(&provider_root, "cursor-ide", "Cursor IDE", Some(&lock_root))
            .expect("provider root lock");
        let other_provider_root = temporary_root.join("other-provider");
        fs::create_dir(&other_provider_root).expect("other provider root");
        let other_guard = acquire(
            &other_provider_root,
            "cursor-ide",
            "Cursor IDE",
            Some(&lock_root),
        )
        .expect("different provider root has independent lock");
        drop(other_guard);
        assert_eq!(
            fs::metadata(&lock_root)
                .expect("lock root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let lock_path = configured_lock_path(&lock_root, &provider_root).expect("lock path");
        assert_eq!(
            fs::metadata(&lock_path)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(guard);
        fs::remove_file(&lock_path).expect("remove regular lock");

        let target = temporary_root.join("symlink-target");
        fs::write(&target, b"").expect("symlink target");
        symlink(&target, &lock_path).expect("symlink lock");
        let error = acquire(&provider_root, "cursor-ide", "Cursor IDE", Some(&lock_root))
            .expect_err("symlink lock must fail closed");
        assert!(error.to_string().contains("must be a regular file"));
        fs::remove_file(&lock_path).expect("remove symlink lock");

        let linked_source = temporary_root.join("linked-lock");
        fs::write(&linked_source, b"").expect("hard-link source");
        fs::set_permissions(&linked_source, fs::Permissions::from_mode(0o644))
            .expect("hard-link source permissions");
        fs::hard_link(&linked_source, &lock_path).expect("hard-link lock");
        let error = acquire(&provider_root, "cursor-ide", "Cursor IDE", Some(&lock_root))
            .expect_err("hard-link lock must fail closed");
        assert!(error.to_string().contains("exactly one link"));
        assert_eq!(
            fs::metadata(&linked_source)
                .expect("hard-link source metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
}
