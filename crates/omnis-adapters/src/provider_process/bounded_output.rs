use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use command_group::{CommandGroup, GroupChild};
use wait_timeout::ChildExt;

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
    let destination = |limit: Option<usize>| {
        if limit.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        }
    };
    command
        .stdout(destination(stdout_limit))
        .stderr(destination(stderr_limit));
    let mut child = command.group_spawn()?;
    let result = collect(&mut child, stdout_limit, stderr_limit, timeout);
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
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    timeout: Duration,
) -> Result<Output> {
    let started = Instant::now();
    let remaining = || timeout.saturating_sub(started.elapsed());
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "provider command timed out");
    let (send, receive) = mpsc::channel();
    if let Some(limit) = stdout_limit {
        let mut stdout = child
            .inner()
            .stdout
            .take()
            .context("missing provider stdout")?;
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
        let stream = receive
            .recv_timeout(remaining())
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => timed_out(),
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::other("provider output reader stopped")
                }
            })??;
        match stream {
            Stream::Stdout(bytes) => stdout = bytes,
            Stream::Stderr(bytes) => stderr = bytes,
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

    #[test]
    fn overflow_and_timeout_stop_the_launcher_and_its_worker() {
        use std::{
            io::{self, Write},
            process::{Command, Stdio},
            time::Duration,
        };

        const ROLE: &str = "OMNI_BOUNDED_OUTPUT_TEST_ROLE";
        const TEST: &str = "provider_process::bounded_output::tests::overflow_and_timeout_stop_the_launcher_and_its_worker";
        let fixture = |role: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture"])
                .env(ROLE, role);
            command
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
            let (reader, mut writer) = io::pipe().unwrap();
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
            // The worker inherits the read end. A broken pipe confirms it has closed too.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while writer.write_all(b"x").is_ok() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "provider worker survived"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
