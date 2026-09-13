//! Runs executables that a test wrote moments ago.

use std::{
    io::ErrorKind,
    process::{Command, Output},
    thread,
    time::Duration,
};

/// Runs `command`, whose program a test wrote moments ago, and returns its output.
///
/// A child that a concurrent test forked while the program was open for writing keeps that
/// descriptor until it execs, and Linux refuses to run the program meanwhile (`ETXTBSY`), so busy
/// executables are retried briefly. After one run succeeds, no forked child still holds a write
/// descriptor, so later runs of the unchanged program cannot find it busy.
pub(crate) fn output_after_write(command: &mut Command) -> Output {
    for _ in 0..50 {
        match command.output() {
            Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                thread::sleep(Duration::from_millis(20));
            }
            result => return result.expect("run freshly written executable"),
        }
    }
    command.output().expect("run freshly written executable")
}
