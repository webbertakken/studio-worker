//! One stage of a training in a child process (`docs/engines/wake-word.md`, "A training"): its
//! events, how its lines are logged, and how its end becomes an outcome.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::error::TrainingError;
use super::TRACE_TARGET;
use crate::engine::torch::stream::{self, Ended, Pipe};

/// Where a training is: a stage, or the worker's own steps around them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Provision,
    Download,
    Prepare,
    Samples,
    Features,
    Train,
    Evaluate,
    Package,
    /// Before a stage: waiting until the machine has the memory it needs.
    WaitingForMemory,
}

impl Phase {
    /// The stages that run in a child process, in order.
    pub const STAGES: [Phase; 5] = [
        Phase::Prepare,
        Phase::Samples,
        Phase::Features,
        Phase::Train,
        Phase::Evaluate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Provision => "provision",
            Phase::Download => "download",
            Phase::Prepare => "prepare",
            Phase::Samples => "samples",
            Phase::Features => "features",
            Phase::Train => "train",
            Phase::Evaluate => "evaluate",
            Phase::Package => "package",
            Phase::WaitingForMemory => "waiting_for_memory",
        }
    }

    /// How long a stage may run before it counts as hung.  A slow run is far quicker: at the
    /// default settings prepare takes 7 minutes once, train under an hour on a 4090.
    pub fn timeout(self) -> Duration {
        let hours = |h: u64| Duration::from_secs(h * 3600);
        match self {
            Phase::Prepare => hours(2),
            Phase::Samples => hours(1),
            Phase::Features => hours(3),
            Phase::Train => hours(8),
            _ => hours(1),
        }
    }
}

/// Why a stage says it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    OutOfMemory,
    DiskFull,
    BadInput,
    Internal,
}

/// One line of a stage's stdout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "event",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Stage {
        name: String,
    },
    Progress {
        stage: String,
        done: u64,
        total: u64,
    },
    Phonemes {
        language: String,
        text: String,
        role: String,
        phonemes: String,
    },
    Done {
        stage: String,
        seconds: f64,
        device_memory_peak_mib: u64,
        /// The most host memory the stage held (`ru_maxrss`); 0 from a trainer that does not say.
        #[serde(default)]
        host_memory_peak_mib: u64,
        /// The most anonymous memory the stage held (sampled `RssAnon`); 0 from an older trainer.
        #[serde(default)]
        anon_memory_peak_mib: u64,
        summary: serde_json::Value,
    },
    Failed {
        class: FailureClass,
        message: String,
    },
}

/// What a finished stage reports.
#[derive(Debug, Clone, PartialEq)]
pub struct StageDone {
    pub seconds: f64,
    pub device_memory_peak_mib: u64,
    pub host_memory_peak_mib: u64,
    pub anon_memory_peak_mib: u64,
    pub summary: serde_json::Value,
}

/// Exit codes a stage ends with besides 0.
const EXIT_OUT_OF_MEMORY: i32 = 2;
const EXIT_BAD_INPUT: i32 = 3;
const EXIT_DISK_FULL: i32 = 4;
/// How often a stage's progress reaches the log.  Safe range 5 to 300 s.
pub const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// How a stderr line is logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineLevel {
    Debug,
    Info,
    Warn,
}

/// Warnings and errors at warn, the stage's own lines and upstream's validation summaries at info,
/// everything else (TensorFlow's chatter, one line per training step) at debug.
pub fn line_level(line: &str) -> LineLevel {
    let trimmed = line.trim_start();
    if trimmed.contains("Mini-Batch #") {
        return LineLevel::Debug;
    }
    let names_a_failure = trimmed.starts_with("Traceback")
        || trimmed.starts_with("ERROR")
        || trimmed.starts_with("E0000")
        || trimmed.starts_with("CRITICAL")
        || trimmed.contains("Error:")
        || trimmed.contains("OOM when allocating")
        || trimmed.contains("out of memory");
    if names_a_failure {
        return LineLevel::Warn;
    }
    let ours = trimmed.starts_with("[wakeword]")
        || trimmed.starts_with("INFO:absl:Step")
        || trimmed.starts_with("INFO:absl:Cutoff")
        || trimmed.starts_with("INFO:absl:AUC")
        || trimmed.starts_with("INFO:absl:Converting")
        || trimmed.starts_with("INFO:absl:Testing");
    if ours {
        LineLevel::Info
    } else {
        LineLevel::Debug
    }
}

/// Turn the end of a stage into its outcome.  `failure` is the stage's own `failed` event, if any.
pub fn outcome(
    phase: Phase,
    ended: Ended,
    done: Option<StageDone>,
    failure: Option<(FailureClass, String)>,
) -> Result<StageDone, TrainingError> {
    let stage = phase.as_str().to_string();
    let (status, tail) = match ended {
        Ended::TimedOut { .. } => {
            return Err(TrainingError::StageTimedOut {
                stage,
                limit: phase.timeout(),
            })
        }
        Ended::Stopped { .. } => return Err(TrainingError::Cancelled),
        Ended::Exited { status, tail } => (status, tail),
    };
    let message = failure
        .as_ref()
        .map(|(_, message)| message.clone())
        .unwrap_or_else(|| "the stage said nothing about why".into());
    let class = failure.as_ref().map(|(class, _)| *class);
    match (status.code(), class, done) {
        (Some(0), _, Some(done)) => Ok(done),
        (Some(EXIT_OUT_OF_MEMORY), _, _) | (_, Some(FailureClass::OutOfMemory), _) => {
            Err(TrainingError::OutOfMemory { stage, message })
        }
        (Some(EXIT_DISK_FULL), _, _) | (_, Some(FailureClass::DiskFull), _) => {
            Err(TrainingError::DiskFull { stage, message })
        }
        (Some(EXIT_BAD_INPUT), _, _) | (_, Some(FailureClass::BadInput), _) => {
            Err(TrainingError::BadInput { stage, message })
        }
        (None, None, _) if killed_outright(&status) => Err(TrainingError::Killed { stage }),
        (Some(0), _, None) => Err(TrainingError::StageFailed {
            stage,
            status: "exit 0".into(),
            message: "the stage ended without saying it was done".into(),
            tail,
        }),
        _ => Err(TrainingError::StageFailed {
            stage,
            status: stream::describe(&status),
            message,
            tail,
        }),
    }
}

/// Ended by SIGKILL: nothing the stage could catch, most often the out-of-memory killer.
fn killed_outright(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal() == Some(9)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

/// Everything one stage run needs.
pub struct StageRun<'a> {
    pub phase: Phase,
    pub python: &'a Path,
    pub request: &'a Path,
    pub env: &'a [(String, String)],
    /// The stage's full log: every line of both pipes.
    pub log_file: PathBuf,
    pub stop: &'a (dyn Fn() -> bool + Sync),
    /// Called with every event as it arrives (progress, phonemes, …).
    pub on_event: &'a mut dyn FnMut(&Event),
}

/// Run one stage to its end.
pub fn run(stage: StageRun<'_>) -> Result<StageDone, TrainingError> {
    let name = stage.phase.as_str();
    let mut command = crate::process::command(stage.python);
    command
        .args(["-m", "wakeword", name])
        .arg(stage.request)
        .envs(stage.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut log_file =
        std::fs::File::create(&stage.log_file).map_err(|e| TrainingError::StageFailed {
            stage: name.into(),
            status: "not started".into(),
            message: format!("creating {}: {e}", stage.log_file.display()),
            tail: String::new(),
        })?;
    tracing::info!(target: TRACE_TARGET, op = "stage", stage = name, "started");
    let started = Instant::now();
    let mut done = None;
    let mut failure = None;
    let mut last_progress: Option<Instant> = None;
    let on_event = stage.on_event;
    let stop = stage.stop;
    let ended = stream::run_until(command, stage.phase.timeout(), stop, |pipe, line| {
        let _ = writeln!(log_file, "{line}");
        match pipe {
            Pipe::Stdout => match serde_json::from_str::<Event>(line) {
                Ok(event) => {
                    log_event(name, &event, &mut last_progress);
                    on_event(&event);
                    match event {
                        Event::Done {
                            seconds,
                            device_memory_peak_mib,
                            host_memory_peak_mib,
                            anon_memory_peak_mib,
                            summary,
                            ..
                        } => {
                            done = Some(StageDone {
                                seconds,
                                device_memory_peak_mib,
                                host_memory_peak_mib,
                                anon_memory_peak_mib,
                                summary,
                            })
                        }
                        Event::Failed { class, message } => failure = Some((class, message)),
                        _ => {}
                    }
                }
                Err(_) => {
                    tracing::debug!(target: TRACE_TARGET, op = "stage_line", stage = name, line)
                }
            },
            Pipe::Stderr => match line_level(line) {
                LineLevel::Warn => {
                    tracing::warn!(target: TRACE_TARGET, op = "stage_line", stage = name, line)
                }
                LineLevel::Info => {
                    tracing::info!(target: TRACE_TARGET, op = "stage_line", stage = name, line)
                }
                LineLevel::Debug => {
                    tracing::debug!(target: TRACE_TARGET, op = "stage_line", stage = name, line)
                }
            },
        }
    })
    .map_err(|e| TrainingError::StageFailed {
        stage: name.into(),
        status: "not started".into(),
        message: format!("running {}: {e}", stage.python.display()),
        tail: String::new(),
    })?;
    let result = outcome(stage.phase, ended, done, failure);
    let seconds = started.elapsed().as_secs_f64();
    match &result {
        Ok(done) => tracing::info!(
            target: TRACE_TARGET,
            op = "stage",
            stage = name,
            seconds,
            device_memory_peak_mib = done.device_memory_peak_mib,
            host_memory_peak_mib = done.host_memory_peak_mib,
            anon_memory_peak_mib = done.anon_memory_peak_mib,
            "finished"
        ),
        Err(error) => tracing::warn!(
            target: TRACE_TARGET,
            op = "stage",
            stage = name,
            seconds,
            reason = error.reason(),
            error = %error,
            "failed"
        ),
    }
    result
}

fn log_event(stage: &str, event: &Event, last_progress: &mut Option<Instant>) {
    match event {
        Event::Progress {
            stage: what,
            done,
            total,
        } => {
            let due = last_progress.is_none_or(|at| at.elapsed() >= PROGRESS_LOG_INTERVAL);
            if due || done == total {
                *last_progress = Some(Instant::now());
                tracing::info!(target: TRACE_TARGET, op = "stage_event", stage, what = %what, done, total, "progress");
            }
        }
        Event::Phonemes {
            language,
            text,
            role,
            phonemes,
        } => tracing::info!(
            target: TRACE_TARGET,
            op = "stage_event",
            stage,
            language = %language,
            role = %role,
            text = %text,
            phonemes = %phonemes,
            "a voice reads {text:?} as /{phonemes}/"
        ),
        Event::Failed { class, message } => tracing::warn!(
            target: TRACE_TARGET,
            op = "stage_event",
            stage,
            class = ?class,
            message = %message,
            "the stage reports a failure"
        ),
        Event::Stage { .. } | Event::Done { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn exited(code: i32) -> Ended {
        Ended::Exited {
            status: status(code),
            tail: "last line".into(),
        }
    }

    fn done() -> StageDone {
        StageDone {
            seconds: 1.0,
            device_memory_peak_mib: 10,
            host_memory_peak_mib: 0,
            anon_memory_peak_mib: 0,
            summary: serde_json::json!({}),
        }
    }

    #[test]
    fn events_read_as_the_trainer_writes_them() {
        let read = |s: &str| serde_json::from_str::<Event>(s).unwrap();
        assert_eq!(
            read(r#"{"event":"stage","name":"train"}"#),
            Event::Stage {
                name: "train".into()
            }
        );
        assert_eq!(
            read(
                r#"{"event":"done","stage":"samples","seconds":5.6,"deviceMemoryPeakMib":2890,"hostMemoryPeakMib":1200,"summary":{"a":1}}"#
            ),
            Event::Done {
                stage: "samples".into(),
                seconds: 5.6,
                device_memory_peak_mib: 2890,
                host_memory_peak_mib: 1200,
                anon_memory_peak_mib: 0,
                summary: serde_json::json!({"a":1})
            }
        );
        assert_eq!(
            read(r#"{"event":"failed","class":"out_of_memory","message":"CUDA out of memory"}"#),
            Event::Failed {
                class: FailureClass::OutOfMemory,
                message: "CUDA out of memory".into()
            }
        );
        assert!(matches!(
            read(r#"{"event":"progress","stage":"x","done":1,"total":2}"#),
            Event::Progress { done: 1, .. }
        ));
        assert!(matches!(
            read(
                r#"{"event":"phonemes","language":"en","text":"hey roona","role":"positive","phonemes":"h"}"#
            ),
            Event::Phonemes { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_stage_killed_without_a_word_names_the_out_of_memory_killer() {
        use std::os::unix::process::ExitStatusExt as _;
        let killed = Ended::Exited {
            status: std::process::ExitStatus::from_raw(9),
            tail: "last line".into(),
        };
        let error = outcome(Phase::Train, killed, None, None).unwrap_err();
        assert_eq!(error.reason(), "killed");
        assert!(
            error.to_string().contains("journalctl -u earlyoom"),
            "{error}"
        );
    }

    #[test]
    fn every_end_becomes_its_named_outcome() {
        assert_eq!(
            outcome(Phase::Train, exited(0), Some(done()), None),
            Ok(done())
        );
        let reason = |ended, failure| {
            outcome(Phase::Samples, ended, None, failure)
                .unwrap_err()
                .reason()
        };
        assert_eq!(reason(exited(2), None), "out_of_memory");
        assert_eq!(
            reason(exited(1), Some((FailureClass::OutOfMemory, "m".into()))),
            "out_of_memory"
        );
        assert_eq!(reason(exited(4), None), "disk_full");
        assert_eq!(
            reason(exited(1), Some((FailureClass::DiskFull, "m".into()))),
            "disk_full"
        );
        assert_eq!(reason(exited(3), None), "bad_input");
        assert_eq!(reason(exited(0), None), "stage_failed");
        assert_eq!(
            reason(exited(1), Some((FailureClass::Internal, "boom".into()))),
            "stage_failed"
        );
        assert_eq!(
            reason(
                Ended::TimedOut {
                    tail: String::new()
                },
                None
            ),
            "stage_timed_out"
        );
        assert_eq!(
            reason(
                Ended::Stopped {
                    tail: String::new()
                },
                None
            ),
            "cancelled"
        );
        let Err(TrainingError::StageFailed {
            status,
            message,
            tail,
            ..
        }) = outcome(
            Phase::Train,
            exited(1),
            None,
            Some((FailureClass::Internal, "boom".into())),
        )
        else {
            panic!("a stage failure")
        };
        assert_eq!(
            (status.as_str(), message.as_str(), tail.as_str()),
            ("exit 1", "boom", "last line")
        );
    }

    #[test]
    fn stage_lines_are_logged_by_what_they_say() {
        assert_eq!(
            line_level("Validation Batch #1: Accuracy = 0.5; Mini-Batch #2"),
            LineLevel::Debug
        );
        assert_eq!(
            line_level("Traceback (most recent call last):"),
            LineLevel::Warn
        );
        assert_eq!(
            line_level("torch.OutOfMemoryError: CUDA out of memory. Tried"),
            LineLevel::Warn
        );
        assert_eq!(
            line_level("[wakeword] en: 800 speakers, voice en-us"),
            LineLevel::Info
        );
        assert_eq!(
            line_level("INFO:absl:Step 500 (nonstreaming): Validation: recall"),
            LineLevel::Info
        );
        assert_eq!(
            line_level("INFO:absl:Cutoff 0.66: frr=0.78; faph=0.000"),
            LineLevel::Info
        );
        assert_eq!(
            line_level("I0000 00:00 cpu_feature_guard.cc:227] This TensorFlow binary"),
            LineLevel::Debug
        );
    }

    #[test]
    fn every_stage_has_a_name_and_a_limit() {
        for phase in Phase::STAGES {
            assert!(
                phase.timeout() >= Duration::from_secs(3600),
                "{}",
                phase.as_str()
            );
        }
        assert_eq!(Phase::Train.timeout(), Duration::from_secs(8 * 3600));
        assert_eq!(
            serde_json::to_string(&Phase::Package).unwrap(),
            "\"package\""
        );
        assert_eq!(Phase::Download.as_str(), "download");
        assert_eq!(Phase::Provision.as_str(), "provision");
    }

    #[test]
    fn progress_reaches_the_log_at_most_every_interval_and_at_its_end() {
        let logs = crate::test_support::capture(|| {
            let mut last = None;
            for done in 1..=5 {
                log_event(
                    "samples",
                    &Event::Progress {
                        stage: "x".into(),
                        done,
                        total: 5,
                    },
                    &mut last,
                );
            }
        });
        assert_eq!(logs.matches("progress").count(), 2, "{logs}");
    }
}
