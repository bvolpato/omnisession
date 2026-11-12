use std::{
    fs::File,
    io::{self, Read},
    os::fd::OwnedFd,
    sync::{Arc, Weak},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{FlockOperation::NonBlockingLockExclusive, Mode, OFlags, flock, open},
    io::Errno,
};
use rustix_openpty::Pty;

pub(super) fn retain(terminal: &Pty) -> io::Result<Arc<OwnedFd>> {
    // Darwin discards queued output when the last slave closes. Open the slave separately
    // so its lock identifies the writer inherited by the launcher and its workers.
    let path = rustix_openpty::rustix::termios::ttyname(&terminal.user, Vec::new())?;
    let slave = open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    flock(&terminal.user, NonBlockingLockExclusive)?;
    rustix::io::ioctl_fionbio(&terminal.controller, true)?;
    Ok(Arc::new(slave))
}

pub(super) fn read(
    file: &mut File,
    retained: &Weak<OwnedFd>,
    buffer: &mut [u8],
) -> io::Result<usize> {
    let interval = Timespec::try_from(super::PROBE_POLL).expect("probe interval fits timespec");
    loop {
        let Some(slave) = retained.upgrade() else {
            return Ok(0);
        };
        match file.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
            result => return result,
        }
        match flock(&slave, NonBlockingLockExclusive) {
            Ok(()) => {
                // The last writer could have written and closed after the first empty read.
                return match file.read(buffer) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
                    result => result,
                };
            }
            Err(Errno::WOULDBLOCK) => (),
            Err(error) => return Err(error.into()),
        }
        poll(&mut [PollFd::new(&*file, PollFlags::IN)], Some(&interval))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn capture_waits_for_stdout_writers_and_preserves_failure_diagnostics() {
        let worker = tempfile::tempdir().unwrap();
        for (script, stdout, stderr, status) in [
            (
                "printf '['; (sleep 0.05; printf ']') 2>/dev/null &",
                "[]",
                "",
                0,
            ),
            (
                // The worker outlives the capture deadline and exits when the test removes its directory.
                r#"
                    (
                        n=0
                        while [ -d "$1" ] && [ "$n" -lt 90 ]; do
                            sleep 1
                            n=$((n+1))
                        done
                    ) </dev/null >/dev/null 2>&1 &
                    printf '{'
                    printf failure >&2
                    exit 7
                "#,
                "{",
                "failure",
                7,
            ),
        ] {
            let output = super::super::bounded_terminal_output(
                Command::new("sh")
                    .args(["-c", script, "sh"])
                    .arg(worker.path()),
                std::time::Duration::from_secs(30),
                16,
                Some(16),
                None,
            )
            .unwrap_or_else(|error| panic!("fixture expecting exit {status}: {error}"));
            assert_eq!(output.status.code(), Some(status));
            assert_eq!(output.stdout, stdout.as_bytes());
            assert_eq!(output.stderr, stderr.as_bytes());
        }
    }

    #[test]
    fn retains_terminal_bytes_when_the_writer_exits_before_the_first_read() {
        for (body, status) in [("[]", 0), ("", 0), ("{", 1)] {
            let terminal = rustix_openpty::openpty(None, None).unwrap();
            let retained = retain(&terminal).unwrap();
            let child = Command::new("sh")
                .args([
                    "-c",
                    r#"printf '%s' "$1"; exit "$2""#,
                    "sh",
                    body,
                    &status.to_string(),
                ])
                .stdout(Stdio::from(terminal.user))
                .status()
                .unwrap();
            assert_eq!(child.code(), Some(status));
            let mut file = File::from(terminal.controller);
            let mut buffer = [0; 16];
            let count = read(&mut file, &Arc::downgrade(&retained), &mut buffer).unwrap();
            assert_eq!(&buffer[..count], body.as_bytes());
            assert_eq!(
                read(&mut file, &Arc::downgrade(&retained), &mut buffer).unwrap(),
                0
            );
        }
    }
}
