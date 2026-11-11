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
    pub(super) error: Option<Arc<ExpectedError>>,
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

type MatchesError = Box<dyn Fn(&[u8], &[u8]) -> bool + Send + Sync>;

// Some providers split a typed failure across stdout and stderr. Keep only as much of
// each stream as the caller accepts for that response.
pub(super) struct ExpectedError {
    code: i32,
    limit: usize,
    recognizes: MatchesError,
    streams: Mutex<Option<ErrorStreams>>,
    stderr_eof: AtomicBool,
}

#[derive(Default)]
struct ErrorStreams {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    matched: Option<bool>,
}

impl ExpectedError {
    pub(super) fn new(
        code: i32,
        limit: usize,
        recognizes: impl Fn(&[u8], &[u8]) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            code,
            limit,
            recognizes: Box::new(recognizes),
            streams: Mutex::new(Some(ErrorStreams::default())),
            stderr_eof: AtomicBool::new(false),
        }
    }

    fn observe(&self, bytes: &[u8], stderr: bool) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(streams) = guard.as_mut() else {
            return;
        };
        let stream = if stderr {
            &mut streams.stderr
        } else {
            &mut streams.stdout
        };
        if bytes.len() > self.limit - stream.len() {
            *guard = None;
            return;
        }
        stream.extend_from_slice(bytes);
        streams.matched = None;
    }

    fn take_match(&self, code: Option<i32>) -> Option<(Vec<u8>, Vec<u8>)> {
        if code != Some(self.code) {
            return None;
        }
        let mut guard = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        let streams = guard.as_mut()?;
        if !*streams
            .matched
            .get_or_insert_with(|| (self.recognizes)(&streams.stdout, &streams.stderr))
        {
            return None;
        }
        let matched = guard.take()?;
        Some((matched.stdout, matched.stderr))
    }
}

struct ObservedOutput<R, F> {
    reader: R,
    observe: F,
}

impl<R: Read, F: FnMut(&[u8], bool)> Read for ObservedOutput<R, F> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let size = self.reader.read(output)?;
        (self.observe)(&output[..size], size == 0 && !output.is_empty());
        Ok(size)
    }
}

pub(super) fn observe_stderr<R: Read>(reader: R, error: Option<Arc<ExpectedError>>) -> impl Read {
    ObservedOutput {
        reader,
        observe: move |bytes: &[u8], eof| {
            if let Some(error) = &error {
                error.observe(bytes, true);
                if eof {
                    error.stderr_eof.store(true, Ordering::Release);
                }
            }
        },
    }
}

impl Response {
    pub(super) fn json_with_error(error: ExpectedError) -> Self {
        Self {
            state: Arc::new(ResponseState {
                error: Some(Arc::new(error)),
                ..ResponseState::default()
            }),
            recognizes: Box::new(super::json_response::complete_json_response()),
            allow_partial_lines: false,
        }
    }

    fn observe(&mut self, bytes: &[u8], eof: bool) {
        if let Some(error) = &self.state.error {
            error.observe(bytes, false);
        }
        if eof {
            self.state.natural_eof.store(true, Ordering::Release);
        }
        if !self.state.ready.load(Ordering::Acquire) && (self.recognizes)(bytes) {
            self.state.ready.store(true, Ordering::Release);
        }
        if self.allow_partial_lines && bytes.contains(&b'\n') {
            self.state.usable_partial.store(true, Ordering::Release);
        }
    }
}

pub(super) fn collect_response(
    child: ProviderProcess,
    stdout: Option<impl Read + Send + 'static>,
    stdout_limit: Option<usize>,
    stderr_limit: Option<usize>,
    timeout: Duration,
    progress: Option<&LargeOutputWait>,
    mut response: Option<Response>,
) -> Result<Output> {
    let state = response.as_ref().map(|rule| Arc::clone(&rule.state));
    collect_or_stop(
        child,
        stdout.map(|reader| ObservedOutput {
            reader,
            observe: move |bytes: &[u8], eof| {
                if let Some(rule) = &mut response {
                    rule.observe(bytes, eof);
                }
            },
        }),
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
    pub(super) fn take_early_error(&self, code: Option<i32>) -> Option<(Vec<u8>, Vec<u8>)> {
        let error = self.error.as_ref()?;
        if self.natural_eof.load(Ordering::Acquire) && error.stderr_eof.load(Ordering::Acquire) {
            return None;
        }
        error.take_match(code)
    }

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
