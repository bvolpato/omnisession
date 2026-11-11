use std::{
    cell::RefCell,
    io::{self, Read},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use command_group::{CommandGroup, GroupChild};
use wait_timeout::ChildExt;

mod json_response;
mod response;
use response::{Response, ResponseState, collect_response, piped_output};
pub use response::{
    bounded_provider_help_output, bounded_provider_line_output, bounded_provider_text_output,
};

/// Holds the caller's signal handler while a provider probe and its workers are active.
pub trait ProviderProbe {
    /// Set by the caller's signal handler when the probe must stop.
    fn cancellation(&self) -> &AtomicBool;
}

type StartProbe = Arc<dyn Fn() -> Option<Box<dyn ProviderProbe>> + Send + Sync>;
thread_local! {
    static PROBES: RefCell<Option<StartProbe>> = const { RefCell::new(None) };
}

/// Installs `start` for provider operations in `run`. A bounded command calls `start`
/// immediately before spawning. It releases the returned guard after group cleanup.
/// Nested calls share the outer scope. On Unix targets without a waitable exit check,
/// a probe can stop its group only while its launcher is still running.
pub fn with_provider_probes<T>(
    start: impl Fn() -> Option<Box<dyn ProviderProbe>> + Send + Sync + 'static,
    run: impl FnOnce() -> T,
) -> T {
    if PROBES.with_borrow(Option::is_some) {
        return run();
    }
    use_probes(Some(Arc::new(start)), run)
}

fn use_probes<T>(start: Option<StartProbe>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<StartProbe>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PROBES.replace(self.0.take());
        }
    }
    let _restore = Restore(PROBES.replace(start));
    run()
}

/// Carries the caller's probe settings into a concurrent lookup.
pub fn inherit_provider_probes<T>(run: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let start = PROBES.with_borrow(Clone::clone);
    move || use_probes(start, run)
}

struct ProviderProcess {
    group: GroupChild,
    // Keep the handler until the process group and its output have been cleaned up.
    probe: Option<Box<dyn ProviderProbe>>,
}

impl ProviderProcess {
    fn inner(&mut self) -> &mut Child {
        self.group.inner()
    }
}

fn check_cancelled(flag: &AtomicBool) -> io::Result<()> {
    if flag.load(Ordering::SeqCst) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "provider probe interrupted",
        ))
    } else {
        Ok(())
    }
}

fn spawn_group(command: &mut Command) -> io::Result<ProviderProcess> {
    let probe = PROBES.with_borrow(|scope| scope.as_ref().and_then(|start| start()));
    if let Some(active) = &probe {
        check_cancelled(active.cancellation())?;
    }
    let mut group = command.group();
    #[cfg(windows)]
    if probe.is_some() {
        // Windows also stops the job if a second interrupt forces an immediate exit.
        group.kill_on_drop(true);
    }
    Ok(ProviderProcess {
        group: group.spawn()?,
        probe,
    })
}

/// Runs a provider command in its own process group or Windows job, with one deadline for
/// the process and its output. The caller configures stdin. A missing limit discards that
/// stream. An oversized stderr diagnostic is discarded while the command continues.
///
/// # Errors
///
/// Returns an error if the process cannot start, stdout exceeds its limit, output cannot
/// be read, or the deadline expires. On capture failure, stops the process group and reaps
/// its leader. Deadline errors have kind `TimedOut`. A nonzero exit is returned in the
/// output status.
pub fn bounded_provider_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
) -> Result<Output> {
    piped_output(command, timeout, stdout_limit, stderr_limit, None)
}

/// Captures bounded JSON output. A complete object or array can finish a read after the
/// launcher exits, even when a worker inherits an output handle. The caller validates the
/// document and checks the exit status.
///
/// # Errors
///
/// Returns process, read, output-limit, or deadline errors.
pub fn bounded_provider_json_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
) -> Result<Output> {
    piped_output(
        command,
        timeout,
        stdout_limit,
        stderr_limit,
        Some(Response::new(json_response::complete_json_response())),
    )
}

#[cfg(unix)]
pub(crate) fn bounded_terminal_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: Option<usize>,
    large_output_timeout: Option<Duration>,
) -> Result<Output> {
    use rustix_openpty::rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

    let terminal = rustix_openpty::openpty(None, None).context("opening provider stdout PTY")?;
    let mut attributes = tcgetattr(&terminal.user)?;
    attributes.make_raw();
    tcsetattr(&terminal.user, OptionalActions::Now, &attributes)?;
    command
        .stdout(terminal.user)
        .stderr(if stderr_limit.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let child = spawn_group(command);
    // Close the parent's copy of the writer so the reader sees the provider exit.
    command.stdout(Stdio::null());
    let progress = large_output_timeout.map(|maximum| {
        Arc::new(LargeOutputWait {
            last_read: Mutex::new(None),
            maximum,
        })
    });
    collect_response(
        child?,
        Some(TerminalOutput {
            file: terminal.controller.into(),
            bytes: 0,
            progress: progress.clone(),
        }),
        Some(stdout_limit),
        stderr_limit,
        timeout,
        progress.as_deref(),
        Some(Response::new(json_response::complete_json_response())),
    )
}

#[cfg(unix)]
struct TerminalOutput {
    file: std::fs::File,
    bytes: usize,
    progress: Option<Arc<LargeOutputWait>>,
}

#[cfg(unix)]
impl Read for TerminalOutput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        const MIB: usize = 1024 * 1024;
        match self.file.read(buffer) {
            Ok(read) => {
                let previous = self.bytes;
                self.bytes += read;
                if self.bytes >= 8 * MIB
                    && previous / MIB != self.bytes / MIB
                    && let Some(progress) = &self.progress
                {
                    *progress
                        .last_read
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
                }
                Ok(read)
            }
            // Linux PTY controllers return EIO after the last writer closes.
            Err(error)
                if error.raw_os_error()
                    == Some(rustix_openpty::rustix::io::Errno::IO.raw_os_error()) =>
            {
                Ok(0)
            }
            result => result,
        }
    }
}

struct LargeOutputWait {
    last_read: Mutex<Option<Instant>>,
    maximum: Duration,
}

impl LargeOutputWait {
    fn allowed(&self, started: Instant, initial: Duration) -> Duration {
        let last = *self
            .last_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(last) = last else {
            return initial;
        };
        let extended = last
            .saturating_duration_since(started)
            .saturating_add(Duration::from_secs(5));
        initial.max(extended.min(self.maximum))
    }
}

#[derive(Clone, Copy, Default)]
struct OutputWait<'a> {
    large: Option<&'a LargeOutputWait>,
    response: Option<&'a ResponseState>,
}

fn collect_or_stop(
    mut process: ProviderProcess,
    stdout: Option<impl Read + Send + 'static>,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    timeout: Duration,
    wait: OutputWait<'_>,
) -> Result<Output> {
    let cancel = process.probe.as_ref().map(|active| active.cancellation());
    let child = &mut process.group;
    let result = collect(
        child,
        stdout,
        stdout_limit,
        stderr_limit,
        timeout,
        cancel,
        wait,
    );
    if result.is_err() {
        // The launcher can delegate to another executable that still holds the pipe.
        if child.kill().is_err() {
            let _ = child.inner().kill();
        }
        let _ = child.inner().wait();
    }
    result
}

enum Stream {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

fn collect(
    child: &mut GroupChild,
    stdout: Option<impl Read + Send + 'static>,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    wait: OutputWait<'_>,
) -> Result<Output> {
    let started = Instant::now();
    let drain_until = std::cell::Cell::new(None::<Instant>);
    let remaining = || {
        let left = wait
            .large
            .map_or(timeout, |progress| progress.allowed(started, timeout))
            .saturating_sub(started.elapsed());
        drain_until.get().map_or(left, |end| {
            left.max(end.saturating_duration_since(Instant::now()))
        })
    };
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "provider command timed out");
    let mut stopped = false;
    let (send, receive) = mpsc::channel();
    if let Some(limit) = stdout_limit {
        let mut stdout = stdout.context("missing provider stdout")?;
        let stdout_send = send.clone();
        thread::Builder::new().spawn(move || {
            let result = read_limited(&mut stdout, limit).and_then(|bytes| {
                bytes.with_context(|| {
                    format!("provider stdout exceeds safe read limit of {limit} bytes")
                })
            });
            let _ = stdout_send.send(result.map(Stream::Stdout));
        })?;
    }

    let readers = usize::from(stdout_limit.is_some()) + usize::from(stderr_limit.is_some());
    if let Some(limit) = stderr_limit {
        let mut stderr = child
            .inner()
            .stderr
            .take()
            .context("missing provider stderr")?;
        thread::Builder::new().spawn(move || {
            let result = read_limited(&mut stderr, limit).and_then(|bytes| {
                if let Some(bytes) = bytes {
                    return Ok(bytes);
                }
                // Keep draining so verbose diagnostics cannot block a valid stdout response.
                io::copy(&mut stderr, &mut io::sink())?;
                Ok(Vec::new())
            });
            let _ = send.send(result.map(Stream::Stderr));
        })?;
    } else {
        drop(send);
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..readers {
        let stream = loop {
            let delay = probe_wait(cancel, remaining(), wait.response.is_some())?;
            match receive.recv_timeout(delay) {
                Ok(stream) => break stream?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let expired = remaining().is_zero();
                    if !stopped
                        && let Some(response) = wait.response
                        && (response.has_answer()
                            || (expired && response.usable_partial.load(Ordering::Acquire)))
                    {
                        let closed = response.natural_eof.load(Ordering::Acquire);
                        if stop_response_workers(child, stderr_limit.is_some())? {
                            stopped = true;
                            response.stopped_early.store(!closed, Ordering::Release);
                            // An exited launcher has a response. Allow its closed pipes to drain.
                            drain_until.set(Some(Instant::now() + Duration::from_secs(1)));
                            continue;
                        }
                    }
                    if expired {
                        return Err(timed_out().into());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("provider output reader stopped").into());
                }
            }
        };
        match stream {
            Stream::Stdout(bytes) => stdout = bytes,
            Stream::Stderr(bytes) => stderr = bytes,
        }
    }
    if (cancel.is_some() || wait.response.is_some()) && !stopped {
        // A worker can finish the response after its launcher exits. Drain the streams first.
        while !stop_exited_probe(child, false)? {
            let wait = probe_wait(cancel, remaining(), true)?;
            if wait.is_zero() {
                return Err(timed_out().into());
            }
            thread::sleep(wait);
        }
    }
    let status = child
        .inner()
        .wait_timeout(remaining())?
        .ok_or_else(timed_out)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn probe_wait(
    cancel: Option<&AtomicBool>,
    remaining: Duration,
    check_response: bool,
) -> io::Result<Duration> {
    if let Some(flag) = cancel {
        check_cancelled(flag)?;
    }
    if cancel.is_some() || check_response {
        return Ok(remaining.min(Duration::from_millis(20)));
    }
    Ok(remaining)
}

#[cfg(not(any(
    target_os = "openbsd",
    target_os = "redox",
    target_os = "horizon",
    target_os = "cygwin"
)))]
fn stop_exited_probe(child: &mut GroupChild, require_success: bool) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

        // Keep the launcher waitable so its process-group ID cannot be reused before kill.
        let pid = Pid::from_child(child.inner());
        let flags = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        let Some(status) = waitid(WaitId::Pid(pid), flags)? else {
            return Ok(false);
        };
        if require_success && status.exit_status() != Some(0) {
            return Ok(false);
        }
    }
    #[cfg(windows)]
    {
        // The Windows job handle keeps its identity after the launcher exits.
        let Some(status) = child.inner().try_wait()? else {
            return Ok(false);
        };
        if require_success && !status.success() {
            return Ok(false);
        }
    }
    match child.kill() {
        // Darwin reports EPERM when the waitable launcher is the only group member.
        // There is no remaining worker that this process can signal.
        #[cfg(target_vendor = "apple")]
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
            Ok(true)
        }
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {
            Ok(true)
        }
        result => result.map(|()| true),
    }
}

#[cfg(any(
    target_os = "openbsd",
    target_os = "redox",
    target_os = "horizon",
    target_os = "cygwin"
))]
fn stop_exited_probe(child: &mut GroupChild, require_success: bool) -> io::Result<bool> {
    // These targets cannot observe a waitable exit. Once try_wait reaps the launcher,
    // another process can reuse its ID, so it is no longer safe to signal the group.
    Ok(child
        .inner()
        .try_wait()?
        .is_some_and(|s| !require_success || s.success()))
}

#[cfg(not(any(
    target_os = "openbsd",
    target_os = "redox",
    target_os = "horizon",
    target_os = "cygwin"
)))]
use stop_exited_probe as stop_response_workers;

#[cfg(any(
    target_os = "openbsd",
    target_os = "redox",
    target_os = "horizon",
    target_os = "cygwin"
))]
#[allow(clippy::unnecessary_wraps)]
fn stop_response_workers(_: &mut GroupChild, _: bool) -> io::Result<bool> {
    // On these systems, keep the leader waitable until the output is closed.
    Ok(false)
}

fn read_limited(reader: &mut impl Read, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(u64::try_from(limit)?)
        .read_to_end(&mut bytes)?;
    // Probe EOF separately so an output exactly at the limit never doubles the buffer.
    let overflow = io::copy(&mut reader.take(1), &mut io::sink())?;
    Ok((overflow == 0).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::read_limited;
    use std::io::Cursor;

    impl super::ProviderProbe for std::sync::Arc<std::sync::atomic::AtomicBool> {
        fn cancellation(&self) -> &std::sync::atomic::AtomicBool {
            self
        }
    }

    #[test]
    fn only_recent_large_reads_extend_the_initial_wait_up_to_the_maximum() {
        use std::{
            sync::Mutex,
            time::{Duration, Instant},
        };
        let started = Instant::now();
        let initial = Duration::from_secs(30);
        for (last_read, allowed) in [(None, 30), (Some(10), 30), (Some(28), 33), (Some(58), 60)] {
            let wait = super::LargeOutputWait {
                last_read: Mutex::new(
                    last_read.map(|seconds| started + Duration::from_secs(seconds)),
                ),
                maximum: Duration::from_secs(60),
            };
            assert_eq!(wait.allowed(started, initial), Duration::from_secs(allowed));
        }
    }

    #[test]
    fn accepts_the_exact_limit_and_stops_at_the_first_extra_byte() {
        assert_eq!(
            read_limited(&mut &b"1234"[..], 4).unwrap().unwrap(),
            b"1234"
        );
        let mut reader = Cursor::new(b"12345678");
        assert!(read_limited(&mut reader, 4).unwrap().is_none());
        assert_eq!(reader.position(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn combined_help_preserves_stream_order_and_enforces_one_byte_limit() {
        use std::{process::Command, time::Duration};

        let capture = |limit| {
            let mut command = Command::new("sh");
            command.args(["-c", "printf ab >&2; printf cd; printf ef >&2"]);
            super::bounded_provider_help_output(&mut command, Duration::from_secs(5), limit, |_| {
                false
            })
            .map(|(output, _)| output)
        };
        let output = capture(6).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"abcdef");
        assert!(capture(5).is_err());
    }

    const ROLE: &str = "OMNI_BOUNDED_OUTPUT_TEST_ROLE";
    const STARTED: &str = "OMNI_BOUNDED_OUTPUT_TEST_STARTED";

    fn fixture(role: &str) -> std::process::Command {
        const TEST: &str = "provider_process::bounded_output::tests::overflow_and_timeout_stop_the_launcher_and_its_worker";
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, role);
        command
    }

    fn assert_worker_stopped(mut writer: std::io::PipeWriter) {
        use std::{
            io::Write,
            time::{Duration, Instant},
        };
        // The worker inherits the read end. A broken pipe confirms it has closed too.
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.write_all(b"x").is_ok() {
            assert!(Instant::now() < deadline, "provider worker survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn overflow_and_timeout_stop_the_launcher_and_its_worker() {
        use std::{
            io::{self, Write},
            process::Stdio,
            time::Duration,
        };

        if let Ok(role) = std::env::var(ROLE) {
            if role == "worker" {
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            let mut worker = fixture("worker")
                .stdin(Stdio::inherit())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            if let Some(path) = std::env::var_os(STARTED) {
                std::fs::write(path, "started").unwrap();
            }
            if role == "early_exit" {
                std::process::exit(0);
            }
            if role == "overflow" {
                io::stdout().write_all(&[b'x'; 8192]).unwrap();
            }
            worker.wait().unwrap();
            return;
        }

        for (role, limit, timeout) in [
            ("overflow", Some(4096), Duration::from_secs(10)),
            ("timeout", Some(4096), Duration::from_secs(1)),
            ("timeout", None, Duration::from_secs(1)),
        ] {
            let (reader, writer) = io::pipe().unwrap();
            let mut command = fixture(role);
            command.stdin(reader);
            let error =
                super::bounded_provider_output(&mut command, timeout, limit, None).unwrap_err();
            drop(command);
            if role == "timeout" {
                assert_eq!(
                    error.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::TimedOut
                );
            } else {
                assert!(
                    format!("{error:#}").contains("exceeds safe read limit"),
                    "{error:#}"
                );
            }
            assert_worker_stopped(writer);
        }
    }
    #[cfg(not(any(
        target_os = "openbsd",
        target_os = "redox",
        target_os = "horizon",
        target_os = "cygwin"
    )))]
    #[test]
    fn inspection_stops_workers_after_early_exit_and_cancellation() {
        use std::{
            io,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
            time::{Duration, Instant},
        };

        let temp = tempfile::tempdir().unwrap();
        for role in ["early_exit", "cancel"] {
            let started = temp.path().join(role);
            let flag = Arc::new(AtomicBool::new(false));
            let (reader, writer) = io::pipe().unwrap();
            let mut command = fixture(role);
            command.stdin(reader).env(STARTED, &started);
            let result = super::with_provider_probes(
                {
                    let flag = Arc::clone(&flag);
                    move || Some(Box::new(Arc::clone(&flag)))
                },
                || {
                    thread::scope(|scope| {
                        let task = scope.spawn(super::inherit_provider_probes(|| {
                            super::bounded_provider_output(
                                &mut command,
                                Duration::from_secs(10),
                                None,
                                None,
                            )
                        }));
                        if role == "cancel" {
                            let deadline = Instant::now() + Duration::from_secs(8);
                            while !started.exists() && Instant::now() < deadline {
                                thread::sleep(Duration::from_millis(10));
                            }
                            flag.store(true, Ordering::SeqCst);
                        }
                        task.join().unwrap()
                    })
                },
            );
            drop(command);
            assert!(started.is_file(), "launcher did not start its worker");
            if role == "cancel" {
                assert_eq!(
                    result
                        .unwrap_err()
                        .downcast_ref::<io::Error>()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::Interrupted
                );
                let retry = temp.path().join("unexpected-retry");
                let mut probe = fixture("early_exit");
                probe.env(STARTED, &retry);
                let rejected = super::with_provider_probes(
                    {
                        let flag = Arc::clone(&flag);
                        move || Some(Box::new(Arc::clone(&flag)))
                    },
                    || {
                        super::bounded_provider_output(
                            &mut probe,
                            Duration::from_secs(5),
                            None,
                            None,
                        )
                    },
                );
                assert!(rejected.is_err());
                assert!(!retry.exists());
            } else {
                assert!(result.unwrap().status.success());
            }
            assert_worker_stopped(writer);
        }
    }
}
