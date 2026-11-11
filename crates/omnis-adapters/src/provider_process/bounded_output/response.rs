use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Result;

use super::{LargeOutputWait, OutputWait, ProviderProcess, collect_or_stop, spawn_group};

#[derive(Default)]
pub(super) struct ResponseState {
    text: Option<Mutex<TextResponse>>,
    pub(super) ready: AtomicBool,
    pub(super) usable_partial: AtomicBool,
    pub(super) natural_eof: AtomicBool,
    pub(super) stopped_early: AtomicBool,
}

type Recognizes = Box<dyn FnMut(&[u8]) -> bool + Send>;

pub(super) struct Response {
    state: Arc<ResponseState>,
    recognizes: Recognizes,
    allow_partial_lines: bool,
}

impl Response {
    pub(super) fn new(recognizes: impl FnMut(&[u8]) -> bool + Send + 'static) -> Self {
        Self {
            state: Arc::default(),
            recognizes: Box::new(recognizes),
            allow_partial_lines: false,
        }
    }
}

struct ObservedOutput<R> {
    reader: R,
    response: Option<Response>,
}

impl<R: Read> Read for ObservedOutput<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let size = self.reader.read(output)?;
        if let Some(rule) = &mut self.response {
            let bytes = &output[..size];
            if size == 0 && !output.is_empty() {
                rule.state.natural_eof.store(true, Ordering::Release);
            }
            if !rule.state.ready.load(Ordering::Acquire) && (rule.recognizes)(bytes) {
                rule.state.ready.store(true, Ordering::Release);
            }
            if rule.allow_partial_lines && bytes.contains(&b'\n') {
                rule.state.usable_partial.store(true, Ordering::Release);
            }
        }
        Ok(size)
    }
}

pub(super) fn collect_response(
    child: ProviderProcess,
    stdout: Option<impl Read + Send + 'static>,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    timeout: Duration,
    progress: Option<&LargeOutputWait>,
    response: Option<Response>,
) -> Result<Output> {
    let state = response.as_ref().map(|rule| Arc::clone(&rule.state));
    collect_or_stop(
        child,
        stdout.map(|reader| ObservedOutput { reader, response }),
        stdout_limit,
        stderr_limit,
        timeout,
        OutputWait {
            large: progress,
            response: state.as_deref(),
        },
    )
}

pub(super) fn piped_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    response: Option<Response>,
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
    let mut child = spawn_group(command)?;
    let stdout = child.inner().stdout.take();
    collect_response(
        child,
        stdout,
        stdout_limit,
        stderr_limit,
        timeout,
        None,
        response,
    )
}

/// Reads a bounded response until EOF or a complete line answers the request and the
/// launcher has exited. The caller checks the exit status.
///
/// # Errors
///
/// Returns an error if the command cannot start, stdout exceeds its limit, or the
/// deadline expires before the response or the launcher is finished.
pub fn bounded_provider_line_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    mut recognizes: impl FnMut(&str) -> bool + Send + 'static,
) -> Result<Output> {
    let mut line = Vec::new();
    let response = Response::new(move |bytes| {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            line.extend_from_slice(part);
            if part.ends_with(b"\n") {
                let ready = std::str::from_utf8(&line).is_ok_and(&mut recognizes);
                line.clear();
                if ready {
                    return true;
                }
            }
        }
        false
    });
    piped_output(command, timeout, Some(stdout_limit), None, Some(response))
}

struct TextResponse {
    recognizes: Box<dyn FnMut(&str) -> bool + Send>,
    bytes: Vec<u8>,
    complete: usize,
    checked: usize,
}

impl ResponseState {
    pub(super) fn has_answer(&self) -> bool {
        if self.ready.load(Ordering::Acquire) {
            return true;
        }
        let Some(text) = &self.text else { return false };
        let mut text = text.lock().unwrap_or_else(PoisonError::into_inner);
        if text.checked == text.complete {
            return false;
        }
        text.checked = text.complete;
        let TextResponse {
            recognizes,
            bytes,
            complete,
            ..
        } = &mut *text;
        let ready = std::str::from_utf8(&bytes[..*complete]).is_ok_and(recognizes);
        self.ready.store(ready, Ordering::Release);
        ready
    }
}

fn text_response(recognizes: impl FnMut(&str) -> bool + Send + 'static) -> Response {
    let state = Arc::new(ResponseState {
        text: Some(Mutex::new(TextResponse {
            recognizes: Box::new(recognizes),
            bytes: Vec::new(),
            complete: 0,
            checked: 0,
        })),
        ..ResponseState::default()
    });
    let reader_state = Arc::clone(&state);
    Response {
        state,
        recognizes: Box::new(move |bytes| {
            let mut text = reader_state
                .text
                .as_ref()
                .expect("text response")
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(last) = bytes.iter().rposition(|byte| *byte == b'\n') {
                text.complete = text.bytes.len() + last + 1;
            }
            text.bytes.extend_from_slice(bytes);
            false
        }),
        allow_partial_lines: false,
    }
}

/// Reads bounded stdout and stderr. Complete stdout lines can establish that all required
/// fields have arrived when a worker inherits a pipe. The caller checks the exit status.
///
/// # Errors
///
/// Returns process, read, output-limit, or deadline errors.
pub fn bounded_provider_text_output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: Option<usize>,
    recognizes: impl FnMut(&str) -> bool + Send + 'static,
) -> Result<Output> {
    piped_output(
        command,
        timeout,
        Some(stdout_limit),
        stderr_limit,
        Some(text_response(recognizes)),
    )
}

/// Reads stdout and stderr in write order under one byte limit. Once a successful launcher
/// exits, the requested flag can complete the probe. At the deadline, complete lines can
/// still establish which lower modes are available. The boolean reports natural EOF so a
/// caller can distinguish a complete list of modes from partial help.
///
/// # Errors
///
/// Returns process, read, output-limit, or deadline errors. The caller checks the exit status.
pub fn bounded_provider_help_output(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    recognizes: impl FnMut(&str) -> bool + Send + 'static,
) -> Result<(Output, bool)> {
    let (reader, writer) = io::pipe()?;
    command.stdout(writer.try_clone()?).stderr(writer);
    let child = spawn_group(command);
    // Drop the command's writer handles so the reader can observe the provider's EOF.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut response = text_response(recognizes);
    response.allow_partial_lines = true;
    let state = Arc::clone(&response.state);
    let mut output = collect_response(
        child?,
        Some(reader),
        Some(limit),
        None,
        timeout,
        None,
        Some(response),
    )?;
    let complete = !state.stopped_early.load(Ordering::Acquire);
    if !complete {
        output.stdout.truncate(
            output
                .stdout
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |last| last + 1),
        );
    }
    Ok((output, complete))
}
