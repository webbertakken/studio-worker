//! The runner: one pipeline, one job, in a child process (`docs/engines/torch.md`, "Runner
//! protocol").  Its stdout carries JSON events, one per line; everything else it prints is log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::error::TorchError;
use super::provision::{Environment, Provisioner};
use super::stream::{self, Ended, Pipe};
use super::TRACE_TARGET;

/// The runner's Python files, embedded so a worker carries the version it was built with.
pub const FILES: [(&str, &str); 6] = [
    ("runner.py", include_str!("../../../python/runner.py")),
    (
        "pipelines/__init__.py",
        include_str!("../../../python/pipelines/__init__.py"),
    ),
    (
        "pipelines/common.py",
        include_str!("../../../python/pipelines/common.py"),
    ),
    (
        "pipelines/pixal3d.py",
        include_str!("../../../python/pipelines/pixal3d.py"),
    ),
    (
        "pipelines/trellis2.py",
        include_str!("../../../python/pipelines/trellis2.py"),
    ),
    (
        "pipelines/hunyuan3d.py",
        include_str!("../../../python/pipelines/hunyuan3d.py"),
    ),
];

/// Exit codes the runner ends with besides 0.
const EXIT_OUT_OF_MEMORY: i32 = 2;
const EXIT_BAD_INPUT: i32 = 3;

/// What the runner is asked to do: `request.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub pipeline: String,
    pub image: PathBuf,
    pub output: PathBuf,
    pub work_dir: PathBuf,
    pub seed: u64,
    pub steps: u32,
    pub guidance: f32,
    pub resolution: Option<u32>,
    pub low_vram: bool,
    pub decimation_target: u32,
    pub texture_size: u32,
    pub budget_gib: f32,
    /// Every source of the environment: its name and checkout.
    pub sources: BTreeMap<String, PathBuf>,
    /// Where pipelines keep the model folders and files they write once.
    pub pipelines_dir: PathBuf,
}

/// Why the runner says it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    OutOfMemory,
    BadInput,
    Internal,
}

/// One line of the runner's stdout.
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
    Done {
        triangles: u64,
        bytes: u64,
        vram_peak_gib: f32,
        seconds: f32,
    },
    Failed {
        class: FailureClass,
        message: String,
    },
}

/// What a finished run reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub triangles: u64,
    pub bytes: u64,
    pub vram_peak_gib: f32,
    pub seconds: f32,
}

/// Write the runner's files into `dir`, rewriting only what differs.
pub fn install(dir: &Path) -> std::io::Result<()> {
    for (name, content) in FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(content) {
            continue;
        }
        std::fs::create_dir_all(path.parent().expect("a runner file lives in a directory"))?;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, content)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(())
}

/// The variables a run sees besides the provisioning ones.
pub fn run_env(provisioner: &Provisioner, runner_dir: &Path) -> Vec<(String, String)> {
    let cache = provisioner.layout.cache();
    let lossy = |p: PathBuf| p.to_string_lossy().into_owned();
    vec![
        ("PYTHONPATH".into(), lossy(runner_dir.to_path_buf())),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("TQDM_DISABLE".into(), "1".into()),
        ("HF_HUB_DISABLE_TELEMETRY".into(), "1".into()),
        ("HF_HUB_DISABLE_PROGRESS_BARS".into(), "1".into()),
        ("ATTN_BACKEND".into(), "sdpa".into()),
        (
            "PYTORCH_CUDA_ALLOC_CONF".into(),
            "expandable_segments:True".into(),
        ),
        ("OPENCV_IO_ENABLE_OPENEXR".into(), "1".into()),
        (
            "FLEX_GEMM_AUTOTUNE_CACHE_PATH".into(),
            lossy(cache.join("flex-gemm-autotune.json")),
        ),
        ("U2NET_HOME".into(), lossy(cache.join("u2net"))),
    ]
}

/// Turn the end of a run into its outcome.  `failure` is the runner's own `failed` event, if any.
pub fn outcome(
    ended: Ended,
    done: Option<Report>,
    failure: Option<(FailureClass, String)>,
    budget_gib: f32,
    timeout: Duration,
) -> Result<Report, TorchError> {
    let (status, tail) = match ended {
        Ended::TimedOut { .. } => return Err(TorchError::Timeout(timeout)),
        Ended::Exited { status, tail } => (status, tail),
    };
    let message = failure
        .as_ref()
        .map(|(_, message)| message.clone())
        .unwrap_or_else(|| "the runner said nothing about why".into());
    match (status.code(), &failure, done) {
        (Some(0), _, Some(report)) => Ok(report),
        (Some(EXIT_OUT_OF_MEMORY), _, _) | (_, Some((FailureClass::OutOfMemory, _)), _) => {
            Err(TorchError::OutOfMemory {
                budget_gib,
                message,
            })
        }
        (Some(EXIT_BAD_INPUT), _, _) | (_, Some((FailureClass::BadInput, _)), _) => {
            Err(TorchError::BadInput(message))
        }
        (Some(0), _, None) => Err(TorchError::Runner {
            status: "exit 0".into(),
            message: "the runner ended without saying it was done".into(),
            tail,
        }),
        _ => Err(TorchError::Runner {
            status: stream::describe(&status),
            message,
            tail,
        }),
    }
}

/// Run `request` in `environment`, streaming its log, until it ends or `timeout` passes.
pub fn run(
    provisioner: &Provisioner,
    environment: &Environment,
    request: &RunRequest,
    timeout: Duration,
) -> Result<Report, TorchError> {
    let runner_dir = provisioner.layout.runner();
    let io = |e: std::io::Error| TorchError::Runner {
        status: "not started".into(),
        message: e.to_string(),
        tail: String::new(),
    };
    install(&runner_dir).map_err(io)?;
    std::fs::create_dir_all(provisioner.layout.cache()).map_err(io)?;
    let request_path = request.work_dir.join("request.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec_pretty(request).expect("a request serialises"),
    )
    .map_err(io)?;
    let mut command = crate::process::command(&environment.python);
    command
        .arg("-u")
        .arg(runner_dir.join("runner.py"))
        .arg(&request_path);
    command.current_dir(&request.work_dir);
    command.envs(provisioner.base_env());
    command.envs(provisioner.build_env(environment));
    command.envs(run_env(provisioner, &runner_dir));
    let mut done = None;
    let mut failure = None;
    let ended = stream::run(command, timeout, |pipe, line| {
        match (pipe, serde_json::from_str::<Event>(line)) {
            (Pipe::Stdout, Ok(Event::Stage { name })) => tracing::info!(
                target: TRACE_TARGET,
                op = "run",
                pipeline = %request.pipeline,
                stage = %name,
                "stage"
            ),
            (
                Pipe::Stdout,
                Ok(Event::Done {
                    triangles,
                    bytes,
                    vram_peak_gib,
                    seconds,
                }),
            ) => {
                done = Some(Report {
                    triangles,
                    bytes,
                    vram_peak_gib,
                    seconds,
                })
            }
            (Pipe::Stdout, Ok(Event::Failed { class, message })) => {
                failure = Some((class, message))
            }
            _ => tracing::info!(target: TRACE_TARGET, op = "runner", "{line}"),
        }
    })
    .map_err(io)?;
    outcome(ended, done, failure, request.budget_gib, timeout)
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

    fn report() -> Report {
        Report {
            triangles: 40_000,
            bytes: 4_000_000,
            vram_peak_gib: 12.5,
            seconds: 90.0,
        }
    }

    #[test]
    fn events_read_the_runners_lines() {
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"event":"stage","name":"generate"}"#).unwrap(),
            Event::Stage {
                name: "generate".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(
                r#"{"event":"done","triangles":40000,"bytes":4000000,"vramPeakGib":12.5,"seconds":90}"#
            )
            .unwrap(),
            Event::Done {
                triangles: 40_000,
                bytes: 4_000_000,
                vram_peak_gib: 12.5,
                seconds: 90.0
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(
                r#"{"event":"failed","class":"out_of_memory","message":"CUDA out of memory"}"#
            )
            .unwrap(),
            Event::Failed {
                class: FailureClass::OutOfMemory,
                message: "CUDA out of memory".into()
            }
        );
        assert!(serde_json::from_str::<Event>("Loading weights").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn every_end_maps_to_its_outcome() {
        let t = Duration::from_secs(60);
        assert_eq!(
            outcome(exited(0), Some(report()), None, 16.0, t).unwrap(),
            report()
        );
        assert!(matches!(
            outcome(exited(2), None, None, 16.0, t),
            Err(TorchError::OutOfMemory { budget_gib, .. }) if budget_gib == 16.0
        ));
        assert!(matches!(
            outcome(
                exited(1),
                None,
                Some((FailureClass::OutOfMemory, "oom".into())),
                16.0,
                t
            ),
            Err(TorchError::OutOfMemory { .. })
        ));
        assert!(matches!(
            outcome(exited(3), None, Some((FailureClass::BadInput, "empty".into())), 16.0, t),
            Err(TorchError::BadInput(m)) if m == "empty"
        ));
        assert!(matches!(
            outcome(exited(0), None, None, 16.0, t),
            Err(TorchError::Runner { message, .. }) if message.contains("without saying")
        ));
        let Err(TorchError::Runner {
            status,
            message,
            tail,
        }) = outcome(
            exited(1),
            None,
            Some((FailureClass::Internal, "boom".into())),
            16.0,
            t,
        )
        else {
            panic!("expected a runner failure")
        };
        assert_eq!(
            (status.as_str(), message.as_str(), tail.as_str()),
            ("exit 1", "boom", "last line")
        );
        assert!(matches!(
            outcome(Ended::TimedOut { tail: String::new() }, None, None, 16.0, t),
            Err(TorchError::Timeout(d)) if d == t
        ));
    }

    #[test]
    fn the_runner_files_are_written_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        for (name, content) in FILES {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                content
            );
        }
        std::fs::write(dir.path().join("runner.py"), "stale").unwrap();
        install(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runner.py")).unwrap(),
            FILES[0].1
        );
    }

    #[test]
    fn a_request_is_camel_case_json() {
        let request = RunRequest {
            pipeline: "pixal3d".into(),
            image: "/w/input.png".into(),
            output: "/w/model.glb".into(),
            work_dir: "/w".into(),
            seed: 42,
            steps: 12,
            guidance: 7.5,
            resolution: Some(1024),
            low_vram: true,
            decimation_target: 40_000,
            texture_size: 1024,
            budget_gib: 16.5,
            sources: BTreeMap::from([("Pixal3D".to_string(), PathBuf::from("/s/p"))]),
            pipelines_dir: "/m/torch/models".into(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["decimationTarget"], 40_000);
        assert_eq!(json["lowVram"], true);
        assert_eq!(json["sources"]["Pixal3D"], "/s/p");
        assert_eq!(json["pipelinesDir"], "/m/torch/models");
    }
}
