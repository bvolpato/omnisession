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
    acquire_with_global_base(
        provider_root,
        namespace,
        provider,
        configured_lock_root,
        None,
    )
}

/// Acquires like [`acquire`], optionally deriving the per-user lock root under `global_base`
/// instead of the platform directory (`/tmp`, `/private/tmp`, or `LOCALAPPDATA`).
///
/// Production always passes `None`. Tests pass a temporary base so they never share the real
/// machine-wide lock root with a running `OmniSession` or another test binary.
pub(crate) fn acquire_with_global_base(
    provider_root: &Path,
    namespace: &str,
    provider: &str,
    configured_lock_root: Option<&Path>,
    global_base: Option<&Path>,
) -> Result<PrivateStoreGuard> {
    let canonical_root = fs::canonicalize(provider_root)
        .with_context(|| format!("canonicalizing {provider} provider root"))?;
    let owner = directory_owner(&canonical_root, provider)?;
    let lock_root = match configured_lock_root {
        Some(path) => normalize_absolute_path(path, provider)?,
        None => user_global_lock_root(global_base, owner, namespace, provider)?,
    };
    if path_starts_with(&lock_root, &canonical_root) {
        bail!("OmniSession {provider} lock directory must be outside provider storage");
    }
    let projected_lock_root = canonicalize_nearest_existing_ancestor(&lock_root, provider)?;
    if path_starts_with(&projected_lock_root, &canonical_root) {
        bail!("OmniSession {provider} lock directory must be outside provider storage");
    }
    ensure_private_lock_directory(&lock_root, owner, provider)?;
    let canonical_lock_root = fs::canonicalize(&lock_root)
        .with_context(|| format!("canonicalizing OmniSession {provider} lock directory"))?;
    if path_starts_with(&canonical_lock_root, &canonical_root) {
        bail!("OmniSession {provider} lock directory must be outside provider storage");
    }
    let lock_path = canonical_lock_root.join(lock_filename(&canonical_root));
    reject_lock_symlink(&lock_path, owner, provider)?;
    let file = open_lock_file(&lock_path, true)
        .with_context(|| format!("opening {provider} provider lock"))?;
    validate_open_lock_file(&lock_path, &file, owner, provider)?;
    file.lock_exclusive()
        .with_context(|| format!("locking {provider} provider root"))?;
    Ok(PrivateStoreGuard { _file: file })
}

fn canonicalize_nearest_existing_ancestor(path: &Path, provider: &str) -> Result<PathBuf> {
    let mut candidate = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(candidate) {
            Ok(_) => {
                let mut projected = fs::canonicalize(candidate).with_context(|| {
                    format!("canonicalizing existing OmniSession {provider} lock ancestor")
                })?;
                for component in missing.iter().rev() {
                    projected.push(component);
                }
                return Ok(projected);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    candidate
                        .file_name()
                        .with_context(|| {
                            format!(
                                "OmniSession {provider} lock path has no existing filesystem ancestor"
                            )
                        })?
                        .to_os_string(),
                );
                candidate = candidate.parent().with_context(|| {
                    format!("OmniSession {provider} lock path has no existing filesystem ancestor")
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "reading existing OmniSession {provider} lock ancestor `{}`",
                        candidate.display()
                    )
                });
            }
        }
    }
}

#[cfg(not(windows))]
fn path_starts_with(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

#[cfg(windows)]
fn path_starts_with(path: &Path, base: &Path) -> bool {
    let mut path_components = path.components();
    base.components().all(|base_component| {
        path_components
            .next()
            .is_some_and(|path_component| windows_component_eq(path_component, base_component))
    })
}

#[cfg(windows)]
fn windows_component_eq(left: std::path::Component<'_>, right: std::path::Component<'_>) -> bool {
    use std::path::{Component, Prefix};

    match (left, right) {
        (Component::Prefix(left), Component::Prefix(right)) => match (left.kind(), right.kind()) {
            (
                Prefix::Disk(left) | Prefix::VerbatimDisk(left),
                Prefix::Disk(right) | Prefix::VerbatimDisk(right),
            ) => left.eq_ignore_ascii_case(&right),
            (
                Prefix::UNC(left_server, left_share) | Prefix::VerbatimUNC(left_server, left_share),
                Prefix::UNC(right_server, right_share)
                | Prefix::VerbatimUNC(right_server, right_share),
            ) => {
                windows_os_str_eq(left_server, right_server)
                    && windows_os_str_eq(left_share, right_share)
            }
            _ => windows_os_str_eq(left.as_os_str(), right.as_os_str()),
        },
        _ => windows_os_str_eq(left.as_os_str(), right.as_os_str()),
    }
}

#[cfg(windows)]
fn windows_os_str_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

#[cfg(not(windows))]
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
}

#[cfg(windows)]
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
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
fn user_global_lock_root(
    global_base: Option<&Path>,
    owner: Option<u32>,
    namespace: &str,
    provider: &str,
) -> Result<PathBuf> {
    let owner = owner.with_context(|| format!("{provider} provider root owner is unavailable"))?;
    Ok(global_base
        .unwrap_or(Path::new("/tmp"))
        .join(format!("omnisession-{owner}/{namespace}")))
}

#[cfg(target_os = "macos")]
fn user_global_lock_root(
    global_base: Option<&Path>,
    owner: Option<u32>,
    namespace: &str,
    provider: &str,
) -> Result<PathBuf> {
    let owner = owner.with_context(|| format!("{provider} provider root owner is unavailable"))?;
    Ok(global_base
        .unwrap_or(Path::new("/private/tmp"))
        .join(format!("omnisession-{owner}/{namespace}")))
}

#[cfg(windows)]
fn user_global_lock_root(
    global_base: Option<&Path>,
    _owner: Option<u32>,
    namespace: &str,
    provider: &str,
) -> Result<PathBuf> {
    let local_app_data = match global_base {
        Some(base) => base.to_path_buf(),
        None => std::env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .with_context(|| format!("{provider} Windows lock root requires LOCALAPPDATA"))?,
    };
    normalize_absolute_path(
        &local_app_data
            .join("OmniSession")
            .join("provider-locks")
            .join(namespace),
        provider,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn user_global_lock_root(
    _global_base: Option<&Path>,
    _owner: Option<u32>,
    _namespace: &str,
    provider: &str,
) -> Result<PathBuf> {
    bail!("native {provider} provider locking is unsupported on this platform")
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
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
                bail!("`{}` is not a safe directory", path.display());
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("reading `{}`", path.display()));
        }
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
        if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
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
        Ok(metadata) if metadata_is_reparse_point(&metadata) || !metadata.is_file() => {
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
    if metadata_is_reparse_point(&path_metadata) || !path_metadata.is_file() {
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
    #[cfg(windows)]
    validate_windows_open_lock_file(path, file, provider)?;
    #[cfg(not(any(unix, windows)))]
    let _ = file;
    Ok(())
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowsFileIdentity {
    volume_serial_number: u64,
    file_index: u64,
}

#[cfg(windows)]
struct WindowsFileState {
    identity: WindowsFileIdentity,
    number_of_links: u64,
    reparse_point: bool,
}

#[cfg(windows)]
fn windows_file_state(file: &fs::File, provider: &str) -> Result<WindowsFileState> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u64 = 0x0000_0400;

    let file_type = winapi_util::file::typ(file)
        .with_context(|| format!("reading opened OmniSession {provider} lock type"))?;
    if !file_type.is_disk() {
        bail!("OmniSession {provider} lock must be a disk file");
    }
    let information = winapi_util::file::information(file)
        .with_context(|| format!("reading opened OmniSession {provider} lock identity"))?;
    Ok(WindowsFileState {
        identity: WindowsFileIdentity {
            volume_serial_number: information.volume_serial_number(),
            file_index: information.file_index(),
        },
        number_of_links: information.number_of_links(),
        reparse_point: information.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

#[cfg(windows)]
fn validate_windows_open_lock_file(path: &Path, file: &fs::File, provider: &str) -> Result<()> {
    let opened = windows_file_state(file, provider)?;
    if opened.reparse_point {
        bail!("OmniSession {provider} lock must not be a reparse point");
    }
    if opened.number_of_links != 1 {
        bail!("OmniSession {provider} lock must have exactly one link");
    }
    let visible_file = open_lock_file(path, false)
        .with_context(|| format!("reopening visible OmniSession {provider} lock"))?;
    let visible = windows_file_state(&visible_file, provider)?;
    if visible.reparse_point {
        bail!("OmniSession {provider} lock must not be a reparse point");
    }
    if visible.number_of_links != 1 {
        bail!("OmniSession {provider} lock must have exactly one link");
    }
    if opened.identity != visible.identity {
        bail!("OmniSession {provider} lock changed while opening");
    }
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
    if !metadata.is_file() || metadata_is_reparse_point(metadata) {
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
        for unit in canonical_root
            .to_string_lossy()
            .to_lowercase()
            .encode_utf16()
        {
            hash.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hash.update(canonical_root.as_os_str().to_string_lossy().as_bytes());
    format!("{}.lock", hex::encode(hash.finalize()))
}

#[cfg(test)]
pub(crate) fn configured_lock_path(lock_root: &Path, provider_root: &Path) -> Result<PathBuf> {
    Ok(lock_root.join(lock_filename(&fs::canonicalize(provider_root)?)))
}

#[cfg(test)]
pub(crate) fn global_lock_path(
    provider_root: &Path,
    namespace: &str,
    provider: &str,
    global_base: &Path,
) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(provider_root)?;
    let owner = directory_owner(&canonical_root, provider)?;
    Ok(
        user_global_lock_root(Some(global_base), owner, namespace, provider)?
            .join(lock_filename(&canonical_root)),
    )
}

/// Signal-driven helpers for tests proving that a writer waits for a provider lock.
///
/// Waiters report that they started, holders release on command, and every wait for an event
/// that must eventually happen uses a generous bound, so slow machines cannot fail the tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        fmt::Debug,
        io::{BufRead, BufReader, Read, Write},
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, RecvTimeoutError},
        thread,
        time::Duration,
    };

    /// Upper bound for events that must eventually happen.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
    /// Window in which a waiter must still be blocked. Slowness only lengthens the wait, so this
    /// can miss a lock that never blocks but cannot fail a working one.
    const BLOCKED_WINDOW: Duration = Duration::from_millis(100);
    const HELD_MARKER: &str = "omnisession-test-lock-held";

    /// Runs `waiter` on its own thread while the caller holds a lock, then calls `release`.
    ///
    /// Asserts that the waiter started, did not finish while the lock was held, and finished
    /// after release. Returns the waiter's result.
    pub(crate) fn assert_waits_for_release<T, W, R>(waiter: W, release: R) -> T
    where
        T: Debug + Send + 'static,
        W: FnOnce() -> T + Send + 'static,
        R: FnOnce(),
    {
        let (started_sender, started) = mpsc::channel();
        let (finished_sender, finished) = mpsc::channel();
        let handle = thread::spawn(move || {
            started_sender.send(()).expect("report waiter start");
            finished_sender
                .send(waiter())
                .expect("report waiter result");
        });
        started
            .recv_timeout(EVENT_TIMEOUT)
            .expect("waiter thread started");
        let early = finished.recv_timeout(BLOCKED_WINDOW);
        release();
        match early {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::panic::resume_unwind(
                handle
                    .join()
                    .expect_err("waiter disconnected without panicking"),
            ),
            Ok(result) => panic!("waiter finished while the lock was held: {result:?}"),
        }
        let result = finished
            .recv_timeout(EVENT_TIMEOUT)
            .expect("waiter unblocked after release");
        handle.join().expect("waiter thread");
        result
    }

    /// Child half of [`LockHolder`]: reports the held lock, then keeps it until stdin closes.
    pub(crate) fn hold_until_released<G>(guard: G) {
        writeln!(std::io::stderr(), "{HELD_MARKER}").expect("report held lock");
        std::io::stdin()
            .read_to_end(&mut Vec::new())
            .expect("wait for release command");
        drop(guard);
    }

    /// Test-binary subprocess holding a provider lock until [`LockHolder::release`].
    pub(crate) struct LockHolder {
        child: Child,
        release: Option<ChildStdin>,
    }

    impl LockHolder {
        /// Runs exact test `test_name` in a child process and waits until it holds the lock.
        ///
        /// The child test must call [`hold_until_released`] once it has acquired the lock.
        pub(crate) fn spawn(test_name: &str, configure: impl FnOnce(&mut Command)) -> Self {
            let mut command =
                Command::new(std::env::current_exe().expect("current test executable"));
            command
                .args(["--exact", test_name, "--nocapture"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            configure(&mut command);
            let mut child = command.spawn().expect("spawn lock holder");
            let release = child.stdin.take();
            let stderr = child.stderr.take().expect("lock holder stderr");
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let mut output = String::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.contains(HELD_MARKER) {
                        let _ = sender.send(None);
                    } else {
                        output.push_str(&line);
                        output.push('\n');
                    }
                }
                let _ = sender.send(Some(output));
            });
            let holder = Self { child, release };
            match receiver.recv_timeout(EVENT_TIMEOUT) {
                Ok(None) => holder,
                Ok(Some(output)) => panic!("lock holder exited before holding the lock:\n{output}"),
                Err(error) => panic!("lock holder did not report a held lock: {error}"),
            }
        }

        /// Closes the child's stdin so it releases the lock, then waits for a clean exit.
        pub(crate) fn release(mut self) {
            drop(self.release.take());
            let status = self.child.wait().expect("wait for lock holder");
            assert!(status.success(), "lock holder failed: {status}");
        }
    }

    impl Drop for LockHolder {
        fn drop(&mut self) {
            if self.release.take().is_some() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[test]
    fn provider_root_lock_subprocess() {
        let Some(provider_root) = env::var_os("OMNI_TEST_PRIVATE_LOCK_ROOT") else {
            return;
        };
        let global_base = env::var_os("OMNI_TEST_PRIVATE_LOCK_GLOBAL_BASE")
            .map(PathBuf::from)
            .expect("subprocess global lock base");
        let guard = acquire_with_global_base(
            Path::new(&provider_root),
            "cursor-ide",
            "Cursor IDE",
            None,
            Some(&global_base),
        )
        .expect("hold provider root lock");
        test_support::hold_until_released(guard);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn default_user_global_lock_root_is_fixed_per_user_and_namespace() {
        #[cfg(target_os = "linux")]
        let expected = "/tmp/omnisession-501/cursor-ide";
        #[cfg(target_os = "macos")]
        let expected = "/private/tmp/omnisession-501/cursor-ide";
        assert_eq!(
            user_global_lock_root(None, Some(501), "cursor-ide", "Cursor IDE")
                .expect("default lock root"),
            Path::new(expected)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn user_global_lock_blocks_across_processes_and_state_homes() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let temporary_root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
        let provider_root = temporary_root.join("provider");
        fs::create_dir(&provider_root).expect("provider root");
        let global_base = temporary_root.join("global-locks");
        let other_state_home = PathBuf::from(format!(
            ".omnisession-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        assert!(!other_state_home.exists());
        let holder = test_support::LockHolder::spawn(
            "private_store_lock::tests::provider_root_lock_subprocess",
            |command| {
                command
                    .env("OMNI_TEST_PRIVATE_LOCK_ROOT", &provider_root)
                    .env("OMNI_TEST_PRIVATE_LOCK_GLOBAL_BASE", &global_base)
                    .env("OMNISESSION_HOME", &other_state_home);
            },
        );

        let waiter_root = provider_root.clone();
        let waiter_base = global_base.clone();
        let guard = test_support::assert_waits_for_release(
            move || {
                acquire_with_global_base(
                    &waiter_root,
                    "cursor-ide",
                    "Cursor IDE",
                    None,
                    Some(&waiter_base),
                )
            },
            || holder.release(),
        )
        .expect("acquire provider root lock after release");
        drop(guard);

        assert!(
            global_lock_path(&provider_root, "cursor-ide", "Cursor IDE", &global_base)
                .expect("user-global lock path")
                .is_file(),
            "lock was not created under the injected user-global root"
        );
        assert!(!provider_root.join(".omnisession.lock").exists());
        assert!(!other_state_home.exists());
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

        let provider_alias = temporary_root.join("provider-alias");
        symlink(&provider_root, &provider_alias).expect("provider alias");
        let aliased_lock_root = provider_alias.join("aliased-locks");
        let error = acquire(
            &provider_root,
            "cursor-ide",
            "Cursor IDE",
            Some(&aliased_lock_root),
        )
        .expect_err("aliased provider lock root must fail before creation");
        assert!(error.to_string().contains("outside provider storage"));
        assert!(!provider_root.join("aliased-locks").exists());
        fs::remove_file(&provider_alias).expect("remove provider alias");

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

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_hard_links() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let temporary_root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
        let provider_root = temporary_root.join("provider");
        let lock_root = temporary_root.join("state/locks/cursor-ide");
        fs::create_dir(&provider_root).expect("provider root");
        fs::create_dir_all(&lock_root).expect("lock root");
        let lock_path = configured_lock_path(&lock_root, &provider_root).expect("lock path");
        let linked_source = temporary_root.join("linked-lock");
        fs::write(&linked_source, b"").expect("hard-link source");
        fs::hard_link(&linked_source, &lock_path).expect("hard-link lock");

        let error = acquire(&provider_root, "cursor-ide", "Cursor IDE", Some(&lock_root))
            .expect_err("hard-link lock must fail closed");
        assert!(error.to_string().contains("exactly one link"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_lock_validation_rejects_changed_identity() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first_path = temporary.path().join("first.lock");
        let second_path = temporary.path().join("second.lock");
        fs::write(&first_path, b"").expect("first lock");
        fs::write(&second_path, b"").expect("second lock");
        let first_file = open_lock_file(&first_path, false).expect("open first lock");

        let error = validate_windows_open_lock_file(&second_path, &first_file, "Cursor IDE")
            .expect_err("changed opened identity must fail closed");
        assert!(error.to_string().contains("changed while opening"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_reparse_point_ancestor() {
        use std::os::windows::fs::symlink_dir;

        let temporary = tempfile::tempdir().expect("temporary root");
        let temporary_root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
        let provider_root = temporary_root.join("provider");
        let real_state_root = temporary_root.join("real-state");
        let linked_state_root = temporary_root.join("linked-state");
        fs::create_dir(&provider_root).expect("provider root");
        fs::create_dir(&real_state_root).expect("real state root");
        match symlink_dir(&real_state_root, &linked_state_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("create directory reparse point: {error}"),
        }

        acquire(
            &provider_root,
            "cursor-ide",
            "Cursor IDE",
            Some(&linked_state_root.join("locks/cursor-ide")),
        )
        .expect_err("reparse-point ancestor must fail closed");
        assert!(!real_state_root.join("locks").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_rejects_reparse_point_file() {
        use std::os::windows::fs::symlink_file;

        let temporary = tempfile::tempdir().expect("temporary root");
        let temporary_root = fs::canonicalize(temporary.path()).expect("canonical temporary root");
        let provider_root = temporary_root.join("provider");
        let lock_root = temporary_root.join("state/locks/cursor-ide");
        fs::create_dir(&provider_root).expect("provider root");
        fs::create_dir_all(&lock_root).expect("lock root");
        let lock_path = configured_lock_path(&lock_root, &provider_root).expect("lock path");
        let target = temporary_root.join("reparse-target");
        fs::write(&target, b"").expect("reparse target");
        match symlink_file(&target, &lock_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("create file reparse point: {error}"),
        }

        let error = acquire(&provider_root, "cursor-ide", "Cursor IDE", Some(&lock_root))
            .expect_err("reparse-point lock file must fail closed");
        assert!(error.to_string().contains("must be a regular file"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_provider_containment_is_case_insensitive() {
        assert!(path_starts_with(
            Path::new(r"C:\Users\Person\Provider\locks"),
            Path::new(r"\\?\c:\users\person\provider"),
        ));
        assert!(!path_starts_with(
            Path::new(r"C:\Users\Person\Provider-Other"),
            Path::new(r"c:\users\person\provider"),
        ));
    }
}
