//! A training as a job (`docs/engines/wake-word.md`, "A training"): the service the local API
//! calls, the thread each training runs on, and the steps it takes from a request to a model.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use super::cutoff::{self, Evaluation, Split};
use super::error::TrainingError;
use super::manifest;
use super::own_voice::{OwnVoice, OwnVoiceRecall};
use super::recipe::{self, Download, ENVIRONMENT};
use super::report::{self, Phonemes, StageTiming};
use super::request::TrainingSpec;
use super::stage::{self, Event, Phase, StageRun};
use super::trainer::{self, Paths};
use super::TRACE_TARGET;
use crate::engine::torch::platform::ComputeCapability;
use crate::engine::torch::provision::Environment;
use crate::job_gate::JobGate;
use crate::job_run::JobRun;
use crate::runtime::{truncate_prompt, CurrentJob, JobOutcome, JobSource, WorkerObservers};
use crate::types::TaskKind;
use crate::voice_samples::{Held, StoreError, VoiceSamples};

/// The job's model, as the Jobs page shows it.
pub const MODEL: &str = "microwakeword";
/// A dataset download that receives nothing for this long has stalled.  Safe range 30 to 600 s.
pub const DOWNLOAD_STALL: Duration = Duration::from_secs(120);
/// One spectrogram of the features: 311 slices of 40 float32 (3.2 s at 10 ms), measured.
pub const SPECTROGRAM_BYTES: u64 = 50_000;
/// One synthetic clip: about a second of 22 050 Hz 16-bit audio.
const SAMPLE_BYTES: u64 = 44_000;
/// Free space kept beyond a training's estimate.
const DISK_MARGIN_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Trainings remembered after they end.
const KEPT_TRAININGS: usize = 50;
/// TensorFlow 2.21's GPU wheels.  Safe range: 6.0 and up.
pub const MIN_COMPUTE_CAPABILITY: ComputeCapability = ComputeCapability { major: 6, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub reason: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrainingResult {
    pub model: PathBuf,
    pub manifest: PathBuf,
    pub report: PathBuf,
    pub report_markdown: PathBuf,
    pub cutoff: f64,
    /// On the testing positives, at the cutoff.
    pub recall: f64,
    /// On the testing ambient sets, pooled, at the cutoff.
    pub false_accepts_per_hour: f64,
    pub target_met: bool,
    /// Recall on the held-out own-voice takes at the cutoff; `None` without recording sets.
    pub own_voice_recall: Option<OwnVoiceRecall>,
}

/// A training, as the API answers it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Training {
    pub id: String,
    pub name: String,
    pub phrase: String,
    pub state: State,
    pub stage: Phase,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output_dir: PathBuf,
    pub failure: Option<Failure>,
    pub result: Option<TrainingResult>,
}

/// Why a training is not started.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Busy,
    UnsupportedPlatform(String),
    InsufficientDisk {
        needed_bytes: u64,
        free_bytes: u64,
    },
    Unavailable(String),
    /// A named recording set does not exist, or the sets cannot be split.
    InvalidVoiceSamples(String),
    /// The request names recording sets, and this worker keeps none.
    VoiceSamplesUnavailable,
}

/// Why a training cannot be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRefusal {
    Unknown,
    NotRunning,
}

/// A built environment and the variables provisioning leaves for its runs.
pub struct Provisioned {
    pub environment: Environment,
    pub env: Vec<(String, String)>,
    /// `uv pip freeze` of the environment.
    pub freeze: Vec<String>,
}

type GpuProbe = dyn Fn() -> Result<ComputeCapability, String> + Send + Sync;
type Provision =
    dyn Fn(&Path, ComputeCapability) -> Result<Provisioned, TrainingError> + Send + Sync;
type Describe = dyn Fn() -> Option<String> + Send + Sync;
type FreeSpace = dyn Fn(&Path) -> Option<u64> + Send + Sync;

/// What a training runs on: [`Backend::system`] is this machine; tests swap parts.
pub struct Backend {
    pub gpu: Box<GpuProbe>,
    pub provision: Box<Provision>,
    pub describe_gpu: Box<Describe>,
    pub free_space: Box<FreeSpace>,
    pub datasets: Vec<Download>,
    pub stall: Duration,
}

impl Backend {
    /// This machine: its GPU, the torch engine's provisioner, the recipe's datasets.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn system() -> Self {
        Self {
            gpu: Box::new(|| {
                let (linux_x86_64, nvidia_smi) = crate::engine::torch::platform::probe_gpu();
                crate::engine::torch::platform::read_gpu(
                    linux_x86_64,
                    nvidia_smi.as_deref(),
                    MIN_COMPUTE_CAPABILITY,
                )
            }),
            provision: Box::new(provision_system),
            describe_gpu: Box::new(describe_system_gpu),
            free_space: Box::new(|dir| fs4::available_space(existing_ancestor(dir)).ok()),
            datasets: recipe::datasets(),
            stall: DOWNLOAD_STALL,
        }
    }
}

/// The nearest directory of `dir` that exists (the models root may not yet).
fn existing_ancestor(dir: &Path) -> &Path {
    dir.ancestors().find(|d| d.exists()).unwrap_or(dir)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn provision_system(
    models_root: &Path,
    capability: ComputeCapability,
) -> Result<Provisioned, TrainingError> {
    use crate::engine::torch::layout::Layout;
    use crate::engine::torch::provision::{CudaSource, Provisioner, UvSource};
    let provisioner = Provisioner {
        layout: Layout::new(models_root),
        capability,
        uv: UvSource::from_env(),
        cuda: CudaSource::default(),
    };
    let failed =
        |e: crate::engine::torch::error::TorchError| TrainingError::ProvisionFailed(e.to_string());
    let environment = provisioner.ensure(&ENVIRONMENT).map_err(failed)?;
    let uv = provisioner.ensure_uv().map_err(failed)?;
    let freeze = crate::process::command(&uv)
        .args(["pip", "freeze", "--python"])
        .arg(&environment.python)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let env = provisioner
        .base_env()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string_lossy().into_owned()))
        .collect();
    Ok(Provisioned {
        environment,
        env,
        freeze,
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn describe_system_gpu() -> Option<String> {
    crate::process::command("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total",
            "--format=csv,noheader",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

struct Entry {
    training: Training,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    models_root: PathBuf,
    paths: Paths,
    observers: WorkerObservers,
    gate: JobGate,
    backend: Backend,
    trainings: Mutex<Vec<Entry>>,
    voice_samples: Option<VoiceSamples>,
}

/// The local API's wake word trainings.
#[derive(Clone)]
pub struct WakeWords {
    inner: Arc<Inner>,
}

/// The disk a training still needs: the downloads not on disk, the datasets not yet prepared, and
/// its own samples and features.
pub fn disk_needed(spec: &TrainingSpec, paths: &Paths, datasets: &[Download]) -> u64 {
    let mut needed = DISK_MARGIN_BYTES;
    for d in datasets.iter().cloned().chain(trainer::generators(spec)) {
        if !paths.download(&d).is_file() {
            needed += d.bytes;
        }
        if paths.prepared_marker(&d).is_some_and(|m| !m.is_file()) {
            needed += d.prepared_bytes;
        }
    }
    let s = &spec.settings;
    let languages = spec.languages.len() as u64;
    let positives = u64::from(s.samples.positive_per_language) * languages;
    let near_misses = u64::from(s.samples.near_miss_per_phrase_per_language)
        * spec.negatives.len() as u64
        * languages;
    // Of every 10 clips: 8 train (10 slides per repetition), 1 validates (10 slides), 1 tests.
    let spectrograms =
        |clips: u64, repetitions: u32| clips * (8 * 10 * u64::from(repetitions) + 10 + 1) / 10;
    needed += (positives + near_misses) * SAMPLE_BYTES;
    needed += (spectrograms(positives, s.features.positive_repetitions)
        + spectrograms(near_misses, s.features.near_miss_repetitions))
        * SPECTROGRAM_BYTES;
    needed
}

impl WakeWords {
    pub fn new(models_root: PathBuf, observers: WorkerObservers, gate: JobGate) -> Self {
        Self::with_backend(models_root, observers, gate, Backend::system())
    }

    pub fn with_backend(
        models_root: PathBuf,
        observers: WorkerObservers,
        gate: JobGate,
        backend: Backend,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                paths: Paths::new(&models_root),
                models_root,
                observers,
                gate,
                backend,
                trainings: Mutex::new(Vec::new()),
                voice_samples: None,
            }),
        }
    }

    /// Train with the operator's recordings in `store` when a request names their sets.
    pub fn with_voice_samples(mut self, store: VoiceSamples) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configured before it is shared")
            .voice_samples = Some(store);
        self
    }

    /// Hold the sets `spec` names and split their takes; `None` when it names none.
    fn own_voice(&self, spec: &TrainingSpec) -> Result<Option<(Held, OwnVoice)>, Refusal> {
        if spec.voice_samples.is_empty() {
            return Ok(None);
        }
        let Some(store) = &self.inner.voice_samples else {
            return Err(Refusal::VoiceSamplesUnavailable);
        };
        let held = store.hold(&spec.voice_samples).map_err(|e| match e {
            StoreError::UnknownSet(set) => {
                Refusal::InvalidVoiceSamples(format!("voiceSamples: no set named {set}"))
            }
            other => Refusal::Unavailable(format!("reading the recordings: {other}")),
        })?;
        let own = OwnVoice::split(&spec.voice_samples, &held.takes)
            .map_err(Refusal::InvalidVoiceSamples)?;
        Ok(Some((held, own)))
    }

    /// Start a training on its own thread, or say why not.
    pub fn start(&self, spec: TrainingSpec) -> Result<Training, Refusal> {
        let inner = &self.inner;
        let refuse = |refusal: Refusal, why: &str| {
            tracing::warn!(target: TRACE_TARGET, op = "wake_word", phrase = %spec.phrase, why, "training refused");
            Err(refusal)
        };
        let capability = match (inner.backend.gpu)() {
            Ok(capability) => capability,
            Err(reason) => return refuse(Refusal::UnsupportedPlatform(reason.clone()), &reason),
        };
        let needed = disk_needed(&spec, &inner.paths, &inner.backend.datasets);
        if let Some(free) = (inner.backend.free_space)(&inner.models_root) {
            if free < needed {
                return refuse(
                    Refusal::InsufficientDisk {
                        needed_bytes: needed,
                        free_bytes: free,
                    },
                    &format!("needs {needed} bytes, {free} free"),
                );
            }
        }
        let own_voice = match self.own_voice(&spec) {
            Ok(own_voice) => own_voice,
            Err(refusal) => {
                let why = format!("{refusal:?}");
                return refuse(refusal, &why);
            }
        };
        let Some(reservation) = inner.gate.try_reserve() else {
            return refuse(Refusal::Busy, "another job holds the worker");
        };
        let id = crate::local::next_job_id();
        let training = Training {
            id: id.clone(),
            name: spec.name.clone(),
            phrase: spec.phrase.clone(),
            state: State::Running,
            stage: Phase::Provision,
            started_at: Utc::now(),
            finished_at: None,
            output_dir: inner.paths.output(&spec.name, &id),
            failure: None,
            result: None,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut trainings = inner.trainings.lock();
            trainings.push(Entry {
                training: training.clone(),
                cancel: cancel.clone(),
            });
            let ended = trainings
                .iter()
                .filter(|e| e.training.state != State::Running)
                .count();
            if ended > KEPT_TRAININGS {
                if let Some(oldest) = trainings
                    .iter()
                    .position(|e| e.training.state != State::Running)
                {
                    trainings.remove(oldest);
                }
            }
        }
        tracing::info!(
            target: TRACE_TARGET,
            op = "wake_word",
            id = %id,
            phrase = %spec.phrase,
            languages = ?spec.languages,
            disk_needed_bytes = needed,
            "training accepted"
        );
        let service = self.clone();
        let started_at = training.started_at;
        let spawned = std::thread::Builder::new()
            .name(format!("wake-word-{id}"))
            .spawn(move || {
                let _reservation = reservation;
                // The hold lasts as long as the training: no upload to its sets, no delete.
                let (_held, own) = match own_voice {
                    Some((held, own)) => (Some(held), Some(own)),
                    None => (None, None),
                };
                service.run(&id, spec, own.as_ref(), capability, started_at, &cancel);
            });
        if let Err(e) = spawned {
            let message = format!("could not start the training's thread: {e}");
            self.end(
                &training.id,
                Err(TrainingError::ProvisionFailed(message.clone())),
            );
            return Err(Refusal::Unavailable(message));
        }
        Ok(training)
    }

    /// One of training `id`'s output files, found on disk (so it outlives a daemon restart).
    pub fn output_file(
        &self,
        id: &str,
        file: &str,
    ) -> Result<super::files::OutputFile, super::files::FileRefusal> {
        super::files::output_file(&self.inner.paths, id, file)
    }

    pub fn get(&self, id: &str) -> Option<Training> {
        self.inner
            .trainings
            .lock()
            .iter()
            .find(|e| e.training.id == id)
            .map(|e| e.training.clone())
    }

    /// Every training this daemon ran, newest first.
    pub fn list(&self) -> Vec<Training> {
        let mut all: Vec<Training> = self
            .inner
            .trainings
            .lock()
            .iter()
            .map(|e| e.training.clone())
            .collect();
        all.reverse();
        all
    }

    /// Ask a running training to stop: its stage's child process is killed (a download in flight
    /// finishes its file first).
    pub fn cancel(&self, id: &str) -> Result<Training, CancelRefusal> {
        let trainings = self.inner.trainings.lock();
        let entry = trainings
            .iter()
            .find(|e| e.training.id == id)
            .ok_or(CancelRefusal::Unknown)?;
        if entry.training.state != State::Running {
            return Err(CancelRefusal::NotRunning);
        }
        entry.cancel.store(true, Ordering::SeqCst);
        tracing::info!(target: TRACE_TARGET, op = "wake_word", id, "cancel requested");
        Ok(entry.training.clone())
    }

    fn set_stage(&self, id: &str, phase: Phase) {
        if let Some(entry) = self
            .inner
            .trainings
            .lock()
            .iter_mut()
            .find(|e| e.training.id == id)
        {
            entry.training.stage = phase;
        }
    }

    fn end(&self, id: &str, outcome: Result<TrainingResult, TrainingError>) {
        let mut trainings = self.inner.trainings.lock();
        let Some(entry) = trainings.iter_mut().find(|e| e.training.id == id) else {
            return;
        };
        let training = &mut entry.training;
        training.finished_at = Some(Utc::now());
        match outcome {
            Ok(result) => {
                training.state = State::Completed;
                training.result = Some(result);
            }
            Err(TrainingError::Cancelled) => {
                training.state = State::Cancelled;
                training.failure = Some(Failure {
                    reason: "cancelled",
                    message: "cancelled".into(),
                });
            }
            Err(error) => {
                training.state = State::Failed;
                training.failure = Some(Failure {
                    reason: error.reason(),
                    message: error.to_string(),
                });
            }
        }
    }

    /// The training's thread: a job from start to end.
    fn run(
        &self,
        id: &str,
        spec: TrainingSpec,
        own: Option<&OwnVoice>,
        capability: ComputeCapability,
        started_at: DateTime<Utc>,
        cancel: &AtomicBool,
    ) {
        let job = JobRun::begin(
            &self.inner.observers,
            CurrentJob {
                job_id: id.to_string(),
                kind: TaskKind::WakeWord,
                model: MODEL.into(),
                prompt: truncate_prompt(&spec.phrase),
                started_at,
                source: JobSource::Local,
            },
        );
        let wall = Instant::now();
        let outcome = job.span().in_scope(|| {
            let outcome = self.train(id, &spec, own, capability, started_at, cancel);
            if own.is_some() {
                remove_own_voice(&self.inner.paths.run(id));
            }
            match &outcome {
                Ok(result) => tracing::info!(
                    target: TRACE_TARGET,
                    op = "wake_word",
                    id,
                    seconds = wall.elapsed().as_secs(),
                    cutoff = result.cutoff,
                    recall = result.recall,
                    false_accepts_per_hour = result.false_accepts_per_hour,
                    model = %result.model.display(),
                    "training completed"
                ),
                Err(error) => tracing::warn!(
                    target: TRACE_TARGET,
                    op = "wake_word",
                    id,
                    seconds = wall.elapsed().as_secs(),
                    reason = error.reason(),
                    error = %error,
                    "training failed"
                ),
            }
            outcome
        });
        let job_outcome = match &outcome {
            Ok(_) => JobOutcome::Completed,
            Err(error) => JobOutcome::Failed {
                reason: format!("{}: {error}", error.reason()),
            },
        };
        self.end(id, outcome);
        job.finish(job_outcome);
    }

    fn train(
        &self,
        id: &str,
        spec: &TrainingSpec,
        own: Option<&OwnVoice>,
        capability: ComputeCapability,
        started_at: DateTime<Utc>,
        cancel: &AtomicBool,
    ) -> Result<TrainingResult, TrainingError> {
        let inner = &self.inner;
        let paths = &inner.paths;
        let mut timings = Vec::new();
        let timed = |phase: Phase, started: Instant, timings: &mut Vec<StageTiming>| {
            timings.push(StageTiming {
                stage: phase.as_str().into(),
                seconds: started.elapsed().as_secs_f64(),
                device_memory_peak_mib: 0,
                host_memory_peak_mib: 0,
            })
        };

        self.set_stage(id, Phase::Provision);
        let started = Instant::now();
        let provisioned = (inner.backend.provision)(&inner.models_root, capability)?;
        timed(Phase::Provision, started, &mut timings);

        self.set_stage(id, Phase::Download);
        let started = Instant::now();
        let mut downloads = inner.backend.datasets.clone();
        downloads.extend(trainer::generators(spec));
        self.download_all(&downloads, cancel)?;
        timed(Phase::Download, started, &mut timings);

        let run_dir = paths.run(id);
        let io = |stage: &str, what: &str| {
            let stage = stage.to_string();
            let what = what.to_string();
            move |e: std::io::Error| io_failure(&stage, &what, e)
        };
        std::fs::create_dir_all(run_dir.join("logs"))
            .map_err(io("prepare", "the run directory"))?;
        crate::engine::torch::runner::install_files(&paths.trainer(), &trainer::FILES)
            .map_err(io("prepare", "the trainer's files"))?;
        let request = run_dir.join("request.json");
        let body = trainer::request_json(
            spec,
            paths,
            &run_dir,
            &provisioned.environment,
            &inner.backend.datasets,
            own,
        );
        std::fs::write(
            &request,
            serde_json::to_vec_pretty(&body).expect("a request serialises"),
        )
        .map_err(io("prepare", "request.json"))?;
        let mut env = provisioned.env.clone();
        env.extend(trainer::stage_env(
            &provisioned.environment,
            &paths.trainer(),
            ENVIRONMENT.python,
        ));

        let mut phonemes: Vec<Phonemes> = Vec::new();
        let stop = || cancel.load(Ordering::SeqCst);
        for phase in Phase::STAGES {
            if stop() {
                return Err(TrainingError::Cancelled);
            }
            self.set_stage(id, phase);
            let mut on_event = |event: &Event| {
                if let Event::Phonemes {
                    language,
                    text,
                    role,
                    phonemes: spoken,
                } = event
                {
                    phonemes.push(Phonemes {
                        language: language.clone(),
                        text: text.clone(),
                        role: role.clone(),
                        phonemes: spoken.clone(),
                    });
                }
            };
            let done = stage::run(StageRun {
                phase,
                python: &provisioned.environment.python,
                request: &request,
                env: &env,
                log_file: run_dir.join("logs").join(format!("{}.log", phase.as_str())),
                stop: &stop,
                on_event: &mut on_event,
            })?;
            timings.push(StageTiming {
                stage: phase.as_str().into(),
                seconds: done.seconds,
                device_memory_peak_mib: done.device_memory_peak_mib,
                host_memory_peak_mib: done.host_memory_peak_mib,
            });
        }

        self.set_stage(id, Phase::Package);
        let started = Instant::now();
        let result = self.package(
            id,
            spec,
            &run_dir,
            started_at,
            phonemes,
            &mut timings,
            started,
            &provisioned,
            own,
        )?;
        for bulk in ["samples", "features"] {
            let dir = run_dir.join(bulk);
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(target: TRACE_TARGET, op = "wake_word", id, dir = %dir.display(), error = %e, "could not remove the run's {bulk}");
                }
            }
        }
        Ok(result)
    }

    fn download_all(
        &self,
        downloads: &[Download],
        cancel: &AtomicBool,
    ) -> Result<(), TrainingError> {
        let stall = self.inner.backend.stall;
        for d in downloads {
            if cancel.load(Ordering::SeqCst) {
                return Err(TrainingError::Cancelled);
            }
            let dest = self.inner.paths.download(d);
            if dest.is_file() {
                tracing::debug!(target: TRACE_TARGET, op = "download", name = %d.name, file = %d.file, "on disk");
                continue;
            }
            tracing::info!(target: TRACE_TARGET, op = "download", name = %d.name, file = %d.file, bytes = d.bytes, url = %d.url, "downloading");
            let started = Instant::now();
            crate::engine::download::download_file_verified_within(
                &d.url,
                &dest,
                Some(&d.sha256),
                stall,
            )
            .map_err(|e| {
                TrainingError::download(&format!("{} ({})", d.name, d.file), &d.url, stall, &e)
            })?;
            tracing::info!(target: TRACE_TARGET, op = "download", name = %d.name, file = %d.file, seconds = started.elapsed().as_secs(), "downloaded");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn package(
        &self,
        id: &str,
        spec: &TrainingSpec,
        run_dir: &Path,
        started_at: DateTime<Utc>,
        phonemes: Vec<Phonemes>,
        timings: &mut Vec<StageTiming>,
        started: Instant,
        provisioned: &Provisioned,
        own: Option<&OwnVoice>,
    ) -> Result<TrainingResult, TrainingError> {
        let invalid = |message: String| TrainingError::InvalidOutput {
            stage: "evaluate".into(),
            message,
        };
        let text = std::fs::read(run_dir.join("evaluation.json"))
            .map_err(|e| invalid(format!("reading evaluation.json: {e}")))?;
        let evaluation: Evaluation =
            serde_json::from_slice(&text).map_err(|e| invalid(format!("evaluation.json: {e}")))?;
        evaluation.check().map_err(invalid)?;
        if let Some(own) = own {
            let measured = evaluation
                .own_voice
                .as_ref()
                .map_or(0, |o| o.held_out.len());
            if measured != own.held_out.len() {
                return Err(invalid(format!(
                    "it measured {measured} held-out own-voice takes of {}",
                    own.held_out.len()
                )));
            }
        }
        let model = PathBuf::from(&evaluation.model.path);
        if !model.is_file() {
            return Err(invalid(format!("no model at {}", model.display())));
        }

        let choice = cutoff::choose(&evaluation, spec.target_false_accepts_per_hour);
        let (count, hours) = evaluation.false_accepts(Split::Testing, choice.index);
        let testing_rate = count as f64 / hours;
        let testing_recall = evaluation.recall(Split::Testing, choice.cutoff);
        if choice.target_met {
            tracing::info!(target: TRACE_TARGET, op = "cutoff", id, cutoff = choice.cutoff, validation_false_accepts_per_hour = choice.validation_false_accepts_per_hour, testing_false_accepts_per_hour = testing_rate, testing_recall, why = %choice.why, "cutoff chosen; target met");
        } else {
            tracing::warn!(target: TRACE_TARGET, op = "cutoff", id, cutoff = choice.cutoff, validation_false_accepts_per_hour = choice.validation_false_accepts_per_hour, testing_false_accepts_per_hour = testing_rate, testing_recall, why = %choice.why, "cutoff chosen; the target was NOT met");
        }

        let out = self.inner.paths.output(&spec.name, id);
        let io = |what: &str| {
            let what = what.to_string();
            move |e: std::io::Error| io_failure("package", &what, e)
        };
        std::fs::create_dir_all(&out).map_err(io("the output directory"))?;
        let model_out = out.join(format!("{}.tflite", spec.name));
        std::fs::copy(&model, &model_out).map_err(io("the model"))?;
        let arena = manifest::tensor_arena_size(evaluation.model.tensor_arena_min_bytes);
        let manifest_out = out.join(format!("{}.json", spec.name));
        let manifest =
            manifest::manifest(spec, choice.cutoff, evaluation.model.tensor_arena_min_bytes);
        std::fs::write(
            &manifest_out,
            serde_json::to_vec_pretty(&manifest).expect("a manifest serialises"),
        )
        .map_err(io("the manifest"))?;

        timings.push(StageTiming {
            stage: Phase::Package.as_str().into(),
            seconds: started.elapsed().as_secs_f64(),
            device_memory_peak_mib: 0,
            host_memory_peak_mib: 0,
        });
        let report = report::build(report::Facts {
            id,
            spec,
            started_at,
            finished_at: Utc::now(),
            evaluation: &evaluation,
            choice: &choice,
            phonemes,
            stages: timings.clone(),
            freeze: provisioned.freeze.clone(),
            gpu: (self.inner.backend.describe_gpu)(),
            datasets: self.inner.backend.datasets.clone(),
            generators: trainer::generators(spec),
            tensor_arena_size: arena,
            own_voice: own,
        });
        let report_out = out.join("report.json");
        std::fs::write(
            &report_out,
            serde_json::to_vec_pretty(&report).expect("a report serialises"),
        )
        .map_err(io("report.json"))?;
        let markdown_out = out.join("report.md");
        std::fs::write(&markdown_out, report::markdown(&report)).map_err(io("report.md"))?;

        Ok(TrainingResult {
            model: model_out,
            manifest: manifest_out,
            report: report_out,
            report_markdown: markdown_out,
            cutoff: choice.cutoff,
            recall: testing_recall,
            false_accepts_per_hour: testing_rate,
            target_met: choice.target_met,
            own_voice_recall: report.own_voice.as_ref().map(|o| o.recall),
        })
    }
}

/// Remove what a training derived from the operator's takes: links to them and their features.
/// Their audio stays only in the store.
fn remove_own_voice(run_dir: &Path) {
    let mut dirs = vec![run_dir.join("own_voice")];
    if let Ok(entries) = std::fs::read_dir(run_dir.join("features")) {
        dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("own_"))
        }));
    }
    for dir in dirs {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(target: TRACE_TARGET, op = "wake_word", dir = %dir.display(), error = %e,
                "could not remove what the training derived from the operator's takes")
            }
        }
    }
}

fn io_failure(stage: &str, what: &str, error: std::io::Error) -> TrainingError {
    if error.kind() == std::io::ErrorKind::StorageFull {
        return TrainingError::DiskFull {
            stage: stage.into(),
            message: format!("{what}: {error}"),
        };
    }
    TrainingError::InvalidOutput {
        stage: stage.into(),
        message: format!("writing {what}: {error}"),
    }
}
