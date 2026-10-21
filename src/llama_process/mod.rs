//! The llama process: llama.cpp runs in a child of the daemon, never in the daemon, because ggml
//! ends its whole process on a CUDA error during generation (`CUDA_CHECK` → `ggml_abort` →
//! `abort()`), e.g. when another process takes the VRAM.  In a child that abort ends one request.
//! Spec: docs/engines/llama.md, "Llama process".
//!
//! [`LlamaProcess`] is the daemon's handle to one process; [`IsolatedLlm`] is a loaded model over
//! one, starting a new process after the last one ended.  [`serve`] is the process's side.

pub mod protocol;
pub mod serve;

pub use protocol::LoadSpec;
use protocol::{encode, FromProcess, ToProcess};

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, trace, warn};

/// Tracing target of the daemon's side of every llama process.
pub const TRACE_TARGET: &str = "studio_worker::llama_process";
/// The hidden subcommand a llama process runs.
pub const SERVE_COMMAND: &str = "llama-serve";
/// How long a cancelled request may take to answer before its process is killed.  A decode step
/// takes well under a second.  Safe range 1..=60 s.
pub const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long a process may take to exit after its stdin closed before it is killed: freeing the
/// weights is quick.  Safe range 1..=30 s.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// llama.cpp error lines kept for an error message.  Safe range 3..=20.
pub const REASON_LINES: usize = 6;
/// Of them, the first ones of the request: llama.cpp names the cause first (`cudaMalloc failed:
/// out of memory`), then what failed because of it.  Safe range 1..`REASON_LINES`.
pub const FIRST_REASON_LINES: usize = 2;
/// Characters kept of each of them.  Safe range 80..=2000.
pub const REASON_LINE_CHARS: usize = 300;
/// How often a waiting request checks whether it was cancelled.
const POLL: Duration = Duration::from_millis(50);
/// How long the stderr lines of a process that ended may take to arrive.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// The program a llama process runs.
#[derive(Debug, Clone)]
pub struct ChildProgram {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
}

impl ChildProgram {
    pub fn new(program: impl Into<PathBuf>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
        }
    }

    /// The daemon's own executable running [`SERVE_COMMAND`], with ggml's `gdb` backtrace off (an
    /// abort would wait about 20 s on it).  On Linux `/proc/self/exe`, so a binary replaced on
    /// disk by an update never serves a daemon of another version.
    pub fn this_executable() -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        let program = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let program = std::env::current_exe()?;
        Ok(Self {
            program,
            args: vec![SERVE_COMMAND.into()],
            env: vec![("GGML_NO_BACKTRACE".into(), "1".into())],
        })
    }
}

/// How long a [`LlamaProcess`] waits on its process; tests shorten it.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub cancel_grace: Duration,
    pub stop_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            cancel_grace: CANCEL_GRACE,
            stop_grace: STOP_GRACE,
        }
    }
}

/// Why a llama process could not serve a request.
#[derive(Debug, thiserror::Error)]
pub enum LlamaProcessError {
    #[error("starting the llama process: {0}")]
    Spawn(std::io::Error),
    /// llama.cpp returned an error; `llama` holds its error lines for that request.
    #[error("{}{error}{}", oom_prefix(llama), in_parens(llama))]
    Failed {
        error: String,
        llama: Vec<String>,
        /// The request would fail on any worker (see `runtime::is_permanent_failure`).
        permanent: bool,
    },
    /// The process ended (an abort, a crash, a kill).
    #[error(
        "{}the llama.cpp process ended ({exit}){}; the daemon is unaffected",
        oom_prefix(llama),
        after_colon(llama)
    )]
    Ended { exit: String, llama: Vec<String> },
    /// The process did not answer a cancel in time and was killed.
    #[error("the llama process did not stop when cancelled and was killed")]
    Unresponsive,
    #[error("the llama process is version {got}, the daemon {expected}")]
    Version { got: String, expected: String },
    #[error("the llama process answered {0} out of turn")]
    Protocol(String),
}

impl LlamaProcessError {
    /// Whether the request would fail on any worker too.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                permanent: true,
                ..
            }
        )
    }
}

fn names_out_of_memory(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.to_ascii_lowercase().contains("out of memory"))
}

fn oom_prefix(lines: &[String]) -> &'static str {
    if names_out_of_memory(lines) {
        "out of GPU memory: "
    } else {
        ""
    }
}

fn in_parens(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!(" (llama.cpp: {})", lines.join("; "))
    }
}

fn after_colon(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!(": {}", lines.join("; "))
    }
}

/// The error lines of a request kept for its error message: the first [`FIRST_REASON_LINES`]
/// (llama.cpp names the cause first) and the last ones, [`REASON_LINES`] in all, each at most
/// [`REASON_LINE_CHARS`] long.
#[derive(Debug, Default)]
struct Reasons {
    first: Vec<String>,
    last: VecDeque<String>,
}

impl Reasons {
    fn push(&mut self, line: &str) {
        let line = line.trim();
        let newest = self.last.back().or(self.first.last());
        if line.is_empty() || newest.is_some_and(|newest| newest.as_str() == shorten(line)) {
            return;
        }
        if self.first.len() < FIRST_REASON_LINES {
            self.first.push(shorten(line));
            return;
        }
        if self.last.len() == REASON_LINES - FIRST_REASON_LINES {
            self.last.pop_front();
        }
        self.last.push_back(shorten(line));
    }

    fn take(&mut self) -> Vec<String> {
        let mut lines = std::mem::take(&mut self.first);
        lines.extend(self.last.drain(..));
        lines
    }
}

fn shorten(line: &str) -> String {
    let mut kept: String = line.chars().take(REASON_LINE_CHARS).collect();
    if line.chars().count() > REASON_LINE_CHARS {
        kept.push('…');
    }
    kept
}

/// What a process's pipes delivered.
enum Line {
    Message(FromProcess),
    /// A line outside the protocol: anything on stderr, or stray stdout.
    Raw {
        stderr: bool,
        text: String,
    },
    StdoutClosed,
    StderrClosed,
}

/// The daemon's handle to one llama process serving one model, one request at a time.  Dropping
/// it closes the process's stdin, so the process frees the model and exits.
pub struct LlamaProcess {
    model: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Line>,
    timing: Timing,
    /// How the process ended, once it has.
    ended: Option<String>,
}

impl LlamaProcess {
    /// Start a process for `spec` and wait until it loaded the model.
    pub fn start(program: &ChildProgram, spec: &LoadSpec) -> Result<Self, LlamaProcessError> {
        Self::start_with(program, spec, Timing::default())
    }

    pub fn start_with(
        program: &ChildProgram,
        spec: &LoadSpec,
        timing: Timing,
    ) -> Result<Self, LlamaProcessError> {
        let started = Instant::now();
        let result = Self::spawn(program, spec, timing).and_then(|mut process| {
            process.send(&ToProcess::Load(spec.clone()));
            match process.await_reply(&|| false, &mut |_| {})? {
                FromProcess::Loaded { version } if version == env!("CARGO_PKG_VERSION") => {
                    Ok(process)
                }
                FromProcess::Loaded { version } => Err(LlamaProcessError::Version {
                    got: version,
                    expected: env!("CARGO_PKG_VERSION").into(),
                }),
                other => Err(LlamaProcessError::Protocol(kind_of(&other).into())),
            }
        });
        match &result {
            Ok(process) => info!(
                target: TRACE_TARGET,
                op = "start",
                model = %spec.model,
                pid = process.child.id(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "llama process loaded its model"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET,
                op = "start",
                model = %spec.model,
                error = %e,
                "llama process could not load its model"
            ),
        }
        result
    }

    fn spawn(
        program: &ChildProgram,
        spec: &LoadSpec,
        timing: Timing,
    ) -> Result<Self, LlamaProcessError> {
        let mut cmd = crate::process::command(&program.program);
        cmd.args(&program.args)
            .envs(program.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(LlamaProcessError::Spawn)?;
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        read_lines(stdout, false, tx.clone());
        read_lines(stderr, true, tx);
        Ok(Self {
            model: spec.model.clone(),
            stdin: child.stdin.take(),
            child,
            lines,
            timing,
            ended: None,
        })
    }

    /// Whether the process has ended; it serves nothing more.
    pub fn has_ended(&self) -> bool {
        self.ended.is_some()
    }

    /// Run one chat completion; see [`crate::host::ChatModel::chat`].
    pub fn chat(
        &mut self,
        params: crate::types::LlmParams,
        cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> Result<serde_json::Value, LlamaProcessError> {
        self.request(&ToProcess::Chat { params }, cancelled, on_piece)
            .and_then(|reply| match reply {
                FromProcess::Done { completion } => Ok(completion),
                other => Err(LlamaProcessError::Protocol(kind_of(&other).into())),
            })
    }

    /// The model's token ids for `text`.
    pub fn tokenize(
        &mut self,
        text: &str,
        add_special: bool,
    ) -> Result<Vec<i32>, LlamaProcessError> {
        let request = ToProcess::Tokenize {
            text: text.to_string(),
            add_special,
        };
        self.request(&request, &|| false, &mut |_| {})
            .and_then(|reply| match reply {
                FromProcess::Tokens { tokens } => Ok(tokens),
                other => Err(LlamaProcessError::Protocol(kind_of(&other).into())),
            })
    }

    fn request(
        &mut self,
        request: &ToProcess,
        cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> Result<FromProcess, LlamaProcessError> {
        if let Some(exit) = &self.ended {
            return Err(LlamaProcessError::Ended {
                exit: exit.clone(),
                llama: vec![],
            });
        }
        self.send(request);
        self.await_reply(cancelled, on_piece)
    }

    /// Write one line; a process that is gone shows up as its stdout closing.
    fn send(&mut self, message: &ToProcess) {
        if let Some(stdin) = &mut self.stdin {
            let written = stdin
                .write_all(encode(message).as_bytes())
                .and_then(|()| stdin.flush());
            if let Err(e) = written {
                debug!(target: TRACE_TARGET, model = %self.model, error = %e, "llama process stdin closed");
            }
        }
    }

    /// Wait for the reply to the request in progress, logging the process's lines on this thread
    /// (so they land in the caller's job) and streaming its pieces.
    fn await_reply(
        &mut self,
        cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> Result<FromProcess, LlamaProcessError> {
        let mut reasons = Reasons::default();
        let mut cancel_sent: Option<Instant> = None;
        loop {
            match self.lines.recv_timeout(POLL) {
                Ok(Line::Message(FromProcess::Log {
                    level,
                    target,
                    message,
                    fields,
                })) => {
                    if level == "ERROR" {
                        reasons.push(&message);
                    }
                    log_from_process(&self.model, &level, &target, &format!("{message}{fields}"));
                }
                Ok(Line::Message(FromProcess::Piece { text })) => on_piece(&text),
                Ok(Line::Message(FromProcess::Failed { error, permanent })) => {
                    return Err(LlamaProcessError::Failed {
                        error,
                        llama: reasons.take(),
                        permanent,
                    })
                }
                Ok(Line::Message(reply)) => return Ok(reply),
                Ok(Line::Raw { stderr, text }) => {
                    if stderr {
                        reasons.push(&text);
                    }
                    log_output(&self.model, stderr, &text);
                }
                Ok(Line::StderrClosed) => {}
                Ok(Line::StdoutClosed) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.end(reasons));
                }
                Err(RecvTimeoutError::Timeout) => match cancel_sent {
                    None if cancelled() => {
                        self.send(&ToProcess::Cancel);
                        cancel_sent = Some(Instant::now());
                    }
                    Some(sent) if sent.elapsed() > self.timing.cancel_grace => {
                        warn!(
                            target: TRACE_TARGET,
                            op = "cancel",
                            model = %self.model,
                            pid = self.child.id(),
                            "llama process did not answer a cancel in time; killing it"
                        );
                        let _ = self.child.kill();
                        let status = self.child.wait().ok();
                        self.ended = Some(status.map_or_else(|| "killed".into(), describe_exit));
                        return Err(LlamaProcessError::Unresponsive);
                    }
                    _ => {}
                },
            }
        }
    }

    /// The process's stdout closed mid-request: reap it, collect its last stderr lines.
    fn end(&mut self, mut reasons: Reasons) -> LlamaProcessError {
        let exit = match wait_for(&mut self.child, self.timing.stop_grace) {
            Some(status) => describe_exit(status),
            None => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                "killed after its output closed".into()
            }
        };
        let deadline = Instant::now() + STDERR_DRAIN;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(Line::Raw { stderr, text }) => {
                    if stderr {
                        reasons.push(&text);
                    }
                    log_output(&self.model, stderr, &text);
                }
                Ok(Line::Message(FromProcess::Log {
                    level,
                    target,
                    message,
                    fields,
                })) => {
                    if level == "ERROR" {
                        reasons.push(&message);
                    }
                    log_from_process(&self.model, &level, &target, &format!("{message}{fields}"));
                }
                Ok(Line::StderrClosed) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let llama = reasons.take();
        error!(
            target: TRACE_TARGET,
            op = "process_died",
            model = %self.model,
            pid = self.child.id(),
            exit = %exit,
            reason = %llama.join("; "),
            "llama process ended during a request; the request fails, the daemon keeps serving"
        );
        self.ended = Some(exit.clone());
        LlamaProcessError::Ended { exit, llama }
    }
}

impl Drop for LlamaProcess {
    fn drop(&mut self) {
        if self.ended.is_some() {
            return;
        }
        self.stdin.take();
        let how = match wait_for(&mut self.child, self.timing.stop_grace) {
            Some(status) => format!("exited ({})", describe_exit(status)),
            None => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                "killed: it had not exited after its stdin closed".into()
            }
        };
        while let Ok(line) = self.lines.try_recv() {
            match line {
                Line::Message(FromProcess::Log {
                    level,
                    target,
                    message,
                    fields,
                }) => log_from_process(&self.model, &level, &target, &format!("{message}{fields}")),
                Line::Raw { stderr, text } => log_output(&self.model, stderr, &text),
                _ => {}
            }
        }
        info!(
            target: TRACE_TARGET,
            op = "stop",
            model = %self.model,
            pid = self.child.id(),
            how = %how,
            "llama process stopped"
        );
    }
}

/// Read `pipe` line by line onto `tx` on a thread of its own; protocol lines only from stdout.
fn read_lines(pipe: impl Read + Send + 'static, stderr: bool, tx: Sender<Line>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf).trim_end().to_string();
                    let line = match (stderr, serde_json::from_str::<FromProcess>(&text)) {
                        (false, Ok(message)) => Line::Message(message),
                        _ => Line::Raw { stderr, text },
                    };
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(if stderr {
            Line::StderrClosed
        } else {
            Line::StdoutClosed
        });
    });
}

/// Poll `child` until it exits or `grace` passes.
fn wait_for(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// `signal 6, SIGABRT` or `exit code 3`.
fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            let name = match signal {
                6 => Some("SIGABRT"),
                7 => Some("SIGBUS"),
                9 => Some("SIGKILL"),
                11 => Some("SIGSEGV"),
                15 => Some("SIGTERM"),
                _ => None,
            };
            return match name {
                Some(name) => format!("signal {signal}, {name}"),
                None => format!("signal {signal}"),
            };
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}

fn kind_of(message: &FromProcess) -> &'static str {
    match message {
        FromProcess::Loaded { .. } => "loaded",
        FromProcess::Piece { .. } => "piece",
        FromProcess::Done { .. } => "done",
        FromProcess::Tokens { .. } => "tokens",
        FromProcess::Failed { .. } => "failed",
        FromProcess::Log { .. } => "log",
    }
}

/// Log one tracing event of the process at its own level.
fn log_from_process(model: &str, level: &str, source: &str, message: &str) {
    match level {
        "ERROR" => error!(target: TRACE_TARGET, model, source, "{message}"),
        "WARN" => warn!(target: TRACE_TARGET, model, source, "{message}"),
        "INFO" => info!(target: TRACE_TARGET, model, source, "{message}"),
        "DEBUG" => debug!(target: TRACE_TARGET, model, source, "{message}"),
        _ => trace!(target: TRACE_TARGET, model, source, "{message}"),
    }
}

fn log_output(model: &str, stderr: bool, text: &str) {
    let pipe = if stderr { "stderr" } else { "stdout" };
    warn!(target: TRACE_TARGET, op = "output", model, pipe, "{text}");
}

/// A loaded model served by a llama process.  When the process ends (an abort), the request
/// fails and the next one starts a new process; the model stays loaded.
pub struct IsolatedLlm {
    program: ChildProgram,
    spec: LoadSpec,
    process: Mutex<Option<LlamaProcess>>,
}

impl IsolatedLlm {
    /// Start the model's process and wait until it loaded.
    pub fn start(program: ChildProgram, spec: LoadSpec) -> Result<Self, LlamaProcessError> {
        let process = LlamaProcess::start(&program, &spec)?;
        Ok(Self {
            program,
            spec,
            process: Mutex::new(Some(process)),
        })
    }

    fn serve<R>(
        &self,
        f: impl FnOnce(&mut LlamaProcess) -> Result<R, LlamaProcessError>,
    ) -> anyhow::Result<R> {
        let mut slot = self.process.lock();
        if slot.as_ref().is_none_or(LlamaProcess::has_ended) {
            *slot = None;
            info!(
                target: TRACE_TARGET,
                op = "restart",
                model = %self.spec.model,
                "the model's llama process ended; starting a new one"
            );
            *slot = Some(LlamaProcess::start(&self.program, &self.spec)?);
        }
        Ok(f(slot.as_mut().expect("started above"))?)
    }
}

impl crate::host::LoadedModel for IsolatedLlm {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_chat(&self) -> Option<&dyn crate::host::ChatModel> {
        Some(self)
    }
}

impl crate::host::ChatModel for IsolatedLlm {
    fn chat(
        &self,
        params: crate::types::LlmParams,
        cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> anyhow::Result<serde_json::Value> {
        self.serve(|process| process.chat(params, cancelled, on_piece))
    }

    fn tokenize(&self, text: &str, add_special: bool) -> anyhow::Result<Vec<i32>> {
        self.serve(|process| process.tokenize(text, add_special))
    }
}

#[cfg(test)]
mod tests;
