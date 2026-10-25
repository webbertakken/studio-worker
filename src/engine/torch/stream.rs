//! Run a child process with its output streamed line by line as it happens, its last lines kept,
//! and a time limit.  Provisioning steps and the runner both run through here.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Last lines kept for an error message.  Safe range 10..=200.
pub const TAIL_LINES: usize = 40;
/// First lines naming an error kept for an error message: a compiler names the cause long before
/// the build tool's traceback ends.  Safe range 1..=50.
pub const FIRST_ERROR_LINES: usize = 12;
/// How long the pipes may stay open after the child exits (a grandchild holding them).
const DRAIN_GRACE: Duration = Duration::from_secs(5);
/// How often the child is checked while it prints nothing.
const POLL: Duration = Duration::from_millis(200);

/// Which pipe a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    Stdout,
    Stderr,
}

/// How a streamed run ended.
#[derive(Debug)]
pub enum Ended {
    Exited {
        status: ExitStatus,
        tail: String,
    },
    TimedOut {
        tail: String,
    },
    /// Asked to stop ([`run_until`]) and killed.
    Stopped {
        tail: String,
    },
}

/// The first [`FIRST_ERROR_LINES`] lines that name an error, and the last [`TAIL_LINES`] lines of
/// both pipes, in the order they arrived.
#[derive(Debug, Default)]
struct Tail {
    first_errors: Vec<String>,
    last: VecDeque<String>,
}

/// Whether a line names an error (`fatal error:`, `error:`, `Error compiling`), not a traceback's
/// frame.
fn names_an_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") && !lower.trim_start().starts_with("file ")
}

impl Tail {
    fn push(&mut self, line: &str) {
        if self.first_errors.len() < FIRST_ERROR_LINES && names_an_error(line) {
            self.first_errors.push(line.to_string());
        }
        if self.last.len() == TAIL_LINES {
            self.last.pop_front();
        }
        self.last.push_back(line.to_string());
    }

    fn text(&self) -> String {
        let last = self.last.iter().cloned().collect::<Vec<_>>().join("\n");
        let earlier: Vec<&String> = self
            .first_errors
            .iter()
            .filter(|line| !self.last.contains(line))
            .collect();
        if earlier.is_empty() {
            return last;
        }
        let earlier: Vec<&str> = earlier.iter().map(|s| s.as_str()).collect();
        format!("{}\n...\n{last}", earlier.join("\n"))
    }
}

/// Read `pipe` into `sender` line by line.  A carriage return ends a line too (progress bars).
fn forward(pipe: impl Read, which: Pipe, sender: mpsc::Sender<(Pipe, String)>) {
    let mut reader = BufReader::new(pipe);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buffer);
                for line in text.split(['\n', '\r']).filter(|l| !l.trim().is_empty()) {
                    if sender.send((which, line.to_string())).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Run `command` to its end, or until `timeout` passes (then it is killed), handing every line to
/// `on_line` as it arrives.
pub fn run(
    command: Command,
    timeout: Duration,
    on_line: impl FnMut(Pipe, &str),
) -> std::io::Result<Ended> {
    run_until(command, timeout, || false, on_line)
}

/// [`run`], also killing the child once `stop` answers true (checked with every line, and every
/// [`POLL`] while the child prints nothing).
pub fn run_until(
    mut command: Command,
    timeout: Duration,
    stop: impl Fn() -> bool,
    mut on_line: impl FnMut(Pipe, &str),
) -> std::io::Result<Ended> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let (sender, receiver) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        readers.push(std::thread::spawn(move || {
            forward(stdout, Pipe::Stdout, sender)
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = sender.clone();
        readers.push(std::thread::spawn(move || {
            forward(stderr, Pipe::Stderr, sender)
        }));
    }
    drop(sender);
    let started = Instant::now();
    let mut tail = Tail::default();
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        let quiet = match receiver.recv_timeout(POLL) {
            Ok((pipe, line)) => {
                tail.push(&line);
                on_line(pipe, &line);
                false
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if exited.is_none() {
                    exited = Some((child.wait()?, Instant::now()));
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => true,
        };
        if exited.is_none() {
            if let Some(status) = child.try_wait()? {
                exited = Some((status, Instant::now()));
            }
        }
        match exited {
            // Lines still arriving after the exit are drained; only a quiet pipe ends the wait.
            Some((_, at)) if quiet && at.elapsed() > DRAIN_GRACE => break,
            Some(_) => {}
            None if started.elapsed() > timeout => {
                // Already gone is fine: the kill only has to make sure it is.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Ended::TimedOut { tail: tail.text() });
            }
            None if stop() => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Ended::Stopped { tail: tail.text() });
            }
            None => {}
        }
    }
    let (status, _) = exited.expect("the loop only breaks once the child exited");
    Ok(Ended::Exited {
        status,
        tail: tail.text(),
    })
}

/// How an exit reads in an error: `exit 1`, or the signal that ended it.
pub fn describe(status: &ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit {code}"),
        None => "no exit code".into(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = crate::process::command("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn every_line_of_both_pipes_arrives_in_order_of_each_pipe() {
        let mut lines = Vec::new();
        let ended = run(
            sh("echo one; echo two >&2; printf 'three\\rfour\\n'; exit 3"),
            Duration::from_secs(10),
            |pipe, line| lines.push((pipe, line.to_string())),
        )
        .unwrap();
        let Ended::Exited { status, tail } = ended else {
            panic!("expected an exit")
        };
        assert_eq!(status.code(), Some(3));
        assert_eq!(describe(&status), "exit 3");
        let stdout: Vec<&str> = lines
            .iter()
            .filter(|(p, _)| *p == Pipe::Stdout)
            .map(|(_, l)| l.as_str())
            .collect();
        assert_eq!(stdout, ["one", "three", "four"]);
        assert!(lines.contains(&(Pipe::Stderr, "two".into())));
        assert!(tail.contains("four"));
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        let ended = run(
            sh("i=0; while [ $i -lt 100 ]; do echo line$i; i=$((i+1)); done"),
            Duration::from_secs(10),
            |_, _| {},
        )
        .unwrap();
        let Ended::Exited { tail, .. } = ended else {
            panic!("expected an exit")
        };
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines.last(), Some(&"line99"));
        assert_eq!(lines.first(), Some(&"line60"));
    }

    #[test]
    fn the_first_errors_are_kept_before_the_tail() {
        let ended = run(
            sh(r#"echo 'x.cpp:1: fatal error: Eigen/Dense: No such file'; i=0; while [ $i -lt 60 ]; do echo '  File "build.py", in run'; i=$((i+1)); done; echo 'RuntimeError: Error compiling objects'"#),
            Duration::from_secs(10),
            |_, _| {},
        )
        .unwrap();
        let Ended::Exited { tail, .. } = ended else {
            panic!("expected an exit")
        };
        assert!(
            tail.starts_with("x.cpp:1: fatal error: Eigen/Dense"),
            "{tail}"
        );
        assert!(tail.contains("\n...\n"), "{tail}");
        assert!(
            tail.ends_with("RuntimeError: Error compiling objects"),
            "{tail}"
        );
        assert!(!tail.starts_with("  File"));
    }

    #[test]
    fn a_run_past_its_limit_is_killed() {
        let started = Instant::now();
        let ended = run(
            sh("echo started; sleep 30"),
            Duration::from_millis(300),
            |_, _| {},
        )
        .unwrap();
        assert!(matches!(ended, Ended::TimedOut { ref tail } if tail == "started"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_run_that_never_stops_printing_still_meets_its_limit() {
        let started = Instant::now();
        let ended = run(
            sh("while true; do echo tick; sleep 0.01; done"),
            Duration::from_millis(500),
            |_, _| {},
        )
        .unwrap();
        assert!(matches!(ended, Ended::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_run_asked_to_stop_is_killed() {
        let started = Instant::now();
        let asked = std::sync::atomic::AtomicBool::new(false);
        let ended = run_until(
            sh("echo started; sleep 30"),
            Duration::from_secs(60),
            || asked.load(std::sync::atomic::Ordering::SeqCst),
            |_, line| {
                if line == "started" {
                    asked.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            },
        )
        .unwrap();
        assert!(matches!(ended, Ended::Stopped { ref tail } if tail == "started"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_signal_is_named() {
        let ended = run(sh("kill -9 $$"), Duration::from_secs(10), |_, _| {}).unwrap();
        let Ended::Exited { status, .. } = ended else {
            panic!("expected an exit")
        };
        assert_eq!(describe(&status), "killed by signal 9");
    }

    #[test]
    fn a_program_that_does_not_exist_is_an_error() {
        let command = crate::process::command("/nonexistent/studio-worker-test");
        assert!(run(command, Duration::from_secs(1), |_, _| {}).is_err());
    }
}
