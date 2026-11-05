//! The converter: `python -m convert <request.json>` in the `convert` environment, one child
//! process per job (`docs/engines/convert.md`, "Converter protocol").  Its stdout carries JSON
//! events, one per line; everything else it prints is log.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::error::ConvertError;
use super::job::RunRequest;
use super::TRACE_TARGET;
use crate::engine::torch::provision::{Environment, Provisioner};
use crate::engine::torch::stream::{self, Ended, Pipe};

/// The converter's Python files, embedded so a worker carries the version it was built with.
pub const FILES: [(&str, &str); 8] = [
    (
        "convert/__init__.py",
        include_str!("../../../python/convert/__init__.py"),
    ),
    (
        "convert/__main__.py",
        include_str!("../../../python/convert/__main__.py"),
    ),
    (
        "convert/converter.py",
        include_str!("../../../python/convert/converter.py"),
    ),
    (
        "convert/prng.py",
        include_str!("../../../python/convert/prng.py"),
    ),
    (
        "convert/recipe.py",
        include_str!("../../../python/convert/recipe.py"),
    ),
    (
        "convert/sampler.py",
        include_str!("../../../python/convert/sampler.py"),
    ),
    (
        "convert/scoring.py",
        include_str!("../../../python/convert/scoring.py"),
    ),
    (
        "convert/synthetic.py",
        include_str!("../../../python/convert/synthetic.py"),
    ),
];

/// The converter's exit code for an unusable source.
const EXIT_BAD_INPUT: i32 = 3;

/// One line of the converter's stdout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Step {
        name: String,
    },
    Progress {
        step: String,
        done: u64,
        total: u64,
    },
    Done {
        seconds: f64,
        bytes: u64,
        sha256: String,
        drawings: u64,
        accuracy: serde_json::Value,
    },
    Failed {
        step: String,
        class: String,
        message: String,
    },
}

/// Turn the end of a run into its outcome: `Ok` when the converter said it was done and exited 0.
pub fn outcome(
    ended: Ended,
    done: bool,
    failure: Option<(String, String, String)>,
    step: &str,
    timeout: Duration,
) -> Result<(), ConvertError> {
    let (status, tail) = match ended {
        Ended::TimedOut { .. } => return Err(ConvertError::Timeout(timeout)),
        Ended::Stopped { tail } => {
            return Err(ConvertError::Step {
                step: step.into(),
                message: "the converter was stopped".into(),
                bad_input: false,
                tail,
            })
        }
        Ended::Exited { status, tail } => (status, tail),
    };
    if status.success() && done {
        return Ok(());
    }
    let (step, class, message) = failure.unwrap_or_else(|| {
        let message = if status.success() {
            "the converter ended without saying it was done".to_string()
        } else {
            format!(
                "the converter ended ({}) without saying why",
                stream::describe(&status)
            )
        };
        (step.to_string(), "internal".to_string(), message)
    });
    Err(ConvertError::Step {
        step,
        message,
        bad_input: class == "bad_input" || status.code() == Some(EXIT_BAD_INPUT),
        tail,
    })
}

/// The variables a run sees besides the provisioning ones.
pub fn run_env(program_dir: &Path) -> Vec<(String, String)> {
    vec![
        (
            "PYTHONPATH".into(),
            program_dir.to_string_lossy().into_owned(),
        ),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("HF_HUB_DISABLE_TELEMETRY".into(), "1".into()),
        ("HF_HUB_DISABLE_PROGRESS_BARS".into(), "1".into()),
        // TensorFlow's C++ start-up chatter (oneDNN, CPU features) is not the job's log.
        ("TF_CPP_MIN_LOG_LEVEL".into(), "2".into()),
        // CPU only, even on a machine with a GPU: the conversion and its gate are CPU work.
        ("CUDA_VISIBLE_DEVICES".into(), String::new()),
    ]
}

/// Run `request` in `environment`, streaming its log, until it ends or `timeout` passes.
pub fn run(
    provisioner: &Provisioner,
    environment: &Environment,
    program_dir: &Path,
    request_path: &Path,
    request: &RunRequest,
    timeout: Duration,
) -> Result<(), ConvertError> {
    let io = |what: &str, e: std::io::Error| ConvertError::Step {
        step: "start".into(),
        message: format!("{what}: {e}"),
        bad_input: false,
        tail: String::new(),
    };
    crate::engine::torch::runner::install_files(program_dir, &FILES)
        .map_err(|e| io("installing the converter", e))?;
    std::fs::write(
        request_path,
        serde_json::to_vec_pretty(request).expect("a request serialises"),
    )
    .map_err(|e| io("writing the request", e))?;
    let mut command = crate::process::command(&environment.python);
    command.arg("-u").arg("-m").arg("convert").arg(request_path);
    command.current_dir(request_path.parent().unwrap_or(program_dir));
    command.envs(provisioner.base_env());
    command.envs(run_env(program_dir));
    let mut step = String::from("start");
    let mut done = false;
    let mut failure = None;
    let ended = stream::run(command, timeout, |pipe, line| {
        match (pipe, serde_json::from_str::<Event>(line)) {
            (Pipe::Stdout, Ok(Event::Step { name })) => {
                tracing::info!(target: TRACE_TARGET, op = "convert", step = %name, "step");
                step = name;
            }
            (Pipe::Stdout, Ok(Event::Progress { step, done, total })) => tracing::info!(
                target: TRACE_TARGET,
                op = "convert",
                step = %step,
                done,
                total,
                "progress"
            ),
            (
                Pipe::Stdout,
                Ok(Event::Done {
                    seconds,
                    bytes,
                    sha256,
                    drawings,
                    accuracy,
                }),
            ) => {
                tracing::info!(
                    target: TRACE_TARGET,
                    op = "convert",
                    seconds,
                    bytes,
                    sha256 = %sha256,
                    drawings,
                    accuracy = %accuracy,
                    "converter done"
                );
                done = true;
            }
            (
                Pipe::Stdout,
                Ok(Event::Failed {
                    step,
                    class,
                    message,
                }),
            ) => failure = Some((step, class, message)),
            _ => tracing::info!(target: TRACE_TARGET, op = "runner", "{line}"),
        }
    })
    .map_err(|e| io("starting the converter", e))?;
    outcome(ended, done, failure, &step, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn exited(code: i32) -> Ended {
        use std::os::unix::process::ExitStatusExt as _;
        Ended::Exited {
            status: std::process::ExitStatus::from_raw(code << 8),
            tail: "last line".into(),
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn events_read_the_converters_lines() {
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"event": "step", "name": "gate"}"#).unwrap(),
            Event::Step {
                name: "gate".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(
                r#"{"event": "failed", "step": "fetch", "class": "bad_input", "message": "404"}"#
            )
            .unwrap(),
            Event::Failed {
                step: "fetch".into(),
                class: "bad_input".into(),
                message: "404".into()
            }
        );
        let done = r#"{"event": "done", "seconds": 1.5, "bytes": 9, "sha256": "ab", "drawings": 3, "accuracy": {}}"#;
        assert!(matches!(
            serde_json::from_str::<Event>(done).unwrap(),
            Event::Done { drawings: 3, .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_run_succeeds_only_when_done_and_exited_zero() {
        assert!(outcome(exited(0), true, None, "references", TIMEOUT).is_ok());
        let silent = outcome(exited(0), false, None, "gate", TIMEOUT).unwrap_err();
        assert!(silent
            .to_string()
            .contains("at `gate`: the converter ended without saying it was done"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_names_the_converters_step_and_class() {
        let failure = Some(("fetch".into(), "bad_input".into(), "404".into()));
        let error = outcome(exited(3), false, failure, "fetch", TIMEOUT).unwrap_err();
        assert!(error.is_permanent());
        assert!(error.to_string().contains("at `fetch`: 404"));
        let failure = Some(("gate".into(), "internal".into(), "HTTP 503".into()));
        assert!(!outcome(exited(1), false, failure, "gate", TIMEOUT)
            .unwrap_err()
            .is_permanent());
        let crashed = outcome(exited(139), false, None, "convert", TIMEOUT).unwrap_err();
        assert!(crashed.to_string().contains("at `convert`"), "{crashed}");
    }

    #[test]
    fn a_timeout_or_a_stop_fails_the_job() {
        let timed_out = outcome(
            Ended::TimedOut {
                tail: String::new(),
            },
            false,
            None,
            "gate",
            TIMEOUT,
        );
        assert!(matches!(timed_out, Err(ConvertError::Timeout(_))));
        let stopped = outcome(
            Ended::Stopped {
                tail: String::new(),
            },
            false,
            None,
            "gate",
            TIMEOUT,
        );
        assert!(stopped.unwrap_err().to_string().contains("stopped"));
    }

    #[test]
    fn every_converter_file_is_embedded() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("python/convert");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                name.ends_with(".py").then(|| format!("convert/{name}"))
            })
            .collect();
        on_disk.sort();
        let mut embedded: Vec<String> = FILES.iter().map(|(name, _)| name.to_string()).collect();
        embedded.sort();
        assert_eq!(embedded, on_disk);
    }
}
