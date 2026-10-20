//! Run a child process with its output streamed line by line as it happens, its last lines kept,
//! and a time limit.  Provisioning steps and the runner both run through here.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Lines kept for an error message.  Safe range 10..=200.
pub const TAIL_LINES: usize = 40;
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
    Exited { status: ExitStatus, tail: String },
    TimedOut { tail: String },
}

/// The last [`TAIL_LINES`] lines of both pipes, in the order they arrived.
#[derive(Debug, Default)]
struct Tail(VecDeque<String>);

impl Tail {
    fn push(&mut self, line: &str) {
        if self.0.len() == TAIL_LINES {
            self.0.pop_front();
        }
        self.0.push_back(line.to_string());
    }

    fn text(&self) -> String {
        self.0.iter().cloned().collect::<Vec<_>>().join("\n")
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
    mut command: Command,
    timeout: Duration,
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
        match receiver.recv_timeout(POLL) {
            Ok((pipe, line)) => {
                tail.push(&line);
                on_line(pipe, &line);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if exited.is_none() {
                    exited = Some((child.wait()?, Instant::now()));
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if exited.is_none() {
            if let Some(status) = child.try_wait()? {
                exited = Some((status, Instant::now()));
            }
        }
        match exited {
            Some((_, at)) if at.elapsed() > DRAIN_GRACE => break,
            Some(_) => {}
            None if started.elapsed() > timeout => {
                // Already gone is fine: the kill only has to make sure it is.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Ended::TimedOut { tail: tail.text() });
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
