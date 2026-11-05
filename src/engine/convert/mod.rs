//! The convert engine: a published classifier converted to ONNX and scored on a gate set, on the
//! CPU, one child process per job (`docs/engines/convert.md`).  It serves the `model_convert`
//! kind: a task in, the `.onnx` file and its conversion report out.
//!
//! It provisions its own Python environment through the torch engine's provisioner (`uv`, pinned
//! wheels, nothing built) and keeps everything under `models_root`.

pub mod environment;
pub mod error;
pub mod job;
pub mod runner;
pub mod synthetic;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};

pub use error::ConvertError;

use super::{Engine, EngineCapabilities, UnsupportedTask};
use crate::engine::torch::layout::Layout;
use crate::engine::torch::provision::{CudaSource, Provisioner, UvSource};
use crate::types::{ModelConvertParams, ModelEngine, ModelSource, Task, TaskKind, TaskResult};

/// Tracing target of the convert engine.  Stable, so operators can filter
/// `RUST_LOG=studio_worker::engine::convert=debug`.
pub const TRACE_TARGET: &str = "studio_worker::engine::convert";
/// The model id the engine advertises for `model_convert`: it serves whatever the registry names
/// with `engine: convert`.
pub const MODEL_SENTINEL: &str = "convert:*";
/// The longest a conversion may take: about 5 minutes for 345 x 100 drawings (fetch, convert,
/// sample, score); a slow dataset host takes longer.  Safe range 10..=55 min.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// Range reads in flight at once: each takes 0.2 to 0.8 s on the dataset's host.
/// Safe range 8..=128.
pub const FETCH_CONCURRENCY: usize = 64;
/// At most this many threads score the gate set (half the machine's, so a GPU job beside it keeps
/// its CPU).  Safe range 1..=32.
pub const SCORE_THREADS_MAX: usize = 8;
/// The largest report the studio stores (`ModelConversionLimits.maxReportBytes`).
pub const MAX_REPORT_BYTES: usize = 1024 * 1024;

/// Where the engine keeps its own files: `<models_root>/convert`.
#[derive(Debug, Clone)]
struct Paths {
    root: PathBuf,
}

impl Paths {
    fn program(&self) -> PathBuf {
        self.root.join("program")
    }

    fn gate_sets(&self) -> PathBuf {
        self.root.join("gate-sets")
    }
}

/// Whether this machine can run conversions, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    Supported,
    Unsupported(String),
}

impl Platform {
    /// Linux x86_64: the `uv` the provisioner downloads and the converter's parent watch.
    pub fn classify(linux_x86_64: bool) -> Self {
        if linux_x86_64 {
            Platform::Supported
        } else {
            Platform::Unsupported("model conversion runs on Linux x86_64 only".into())
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn probe() -> Self {
        let platform = Self::classify(cfg!(all(target_os = "linux", target_arch = "x86_64")));
        match &platform {
            Platform::Supported => tracing::info!(
                target: TRACE_TARGET,
                op = "platform",
                "model_convert advertised (CPU)"
            ),
            Platform::Unsupported(reason) => tracing::info!(
                target: TRACE_TARGET,
                op = "platform",
                reason = %reason,
                "model_convert not advertised"
            ),
        }
        platform
    }
}

pub struct ConvertEngine {
    platform: Platform,
    layout: Layout,
    paths: Paths,
    uv: UvSource,
    run_timeout: Duration,
    categories_url: String,
    dataset_url: String,
}

impl ConvertEngine {
    /// The engine for this machine.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn new(models_root: &Path) -> Self {
        Self::with_platform(models_root, Platform::probe())
    }

    /// The engine for a known platform, with the pinned sources.
    pub fn with_platform(models_root: &Path, platform: Platform) -> Self {
        Self {
            platform,
            layout: Layout::new(models_root),
            paths: Paths {
                root: models_root.join("convert"),
            },
            uv: UvSource::from_env(),
            run_timeout: RUN_TIMEOUT,
            categories_url: environment::CATEGORIES_URL.into(),
            dataset_url: environment::QUICKDRAW_RAW_URL.into(),
        }
    }

    /// Replace where `uv` comes from and the run limit (tests).
    pub fn with_sources(mut self, uv: UvSource, run_timeout: Duration) -> Self {
        self.uv = uv;
        self.run_timeout = run_timeout;
        self
    }

    /// Replace where the categories and the gate set's drawings come from (tests, mirrors).
    pub fn with_dataset(mut self, categories_url: String, dataset_url: String) -> Self {
        self.categories_url = categories_url;
        self.dataset_url = dataset_url;
        self
    }

    fn convert(&self, model: &str, task: ModelConvertParams) -> Result<TaskResult> {
        if self.platform != Platform::Supported {
            return Err(UnsupportedTask::new("convert", TaskKind::ModelConvert).into());
        }
        job::check(&task)?;
        let started = Instant::now();
        tracing::info!(
            target: TRACE_TARGET,
            op = "convert",
            model,
            source = %task.source,
            file = %task.file,
            opset = task.opset,
            seed = task.gate_set.seed,
            drawings_per_word = task.gate_set.drawings_per_word,
            "conversion started"
        );
        let provisioner = Provisioner {
            layout: self.layout.clone(),
            capability: None,
            uv: self.uv.clone(),
            cuda: CudaSource::default(),
        };
        let environment = provisioner
            .ensure(&environment::ENVIRONMENT)
            .map_err(ConvertError::from)?;
        let work = tempfile::Builder::new()
            .prefix("studio-worker-convert-")
            .tempdir()
            .context("creating the job's temporary directory")?;
        let request = job::RunRequest {
            task,
            categories_url: self.categories_url.clone(),
            dataset_url: self.dataset_url.clone(),
            output: work.path().join("model.onnx"),
            report: work.path().join("report.json"),
            gate_cache: self.paths.gate_sets(),
            concurrency: FETCH_CONCURRENCY,
            threads: score_threads(),
        };
        let result = runner::run(
            &provisioner,
            &environment,
            &self.paths.program(),
            &work.path().join("request.json"),
            &request,
            self.run_timeout,
        )
        .and_then(|()| read_result(&request));
        match &result {
            Ok(TaskResult::ModelConvert { bytes, report, .. }) => tracing::info!(
                target: TRACE_TARGET,
                op = "convert",
                model,
                seconds = started.elapsed().as_secs_f32(),
                bytes = bytes.len(),
                sha256 = %report["sha256"].as_str().unwrap_or_default(),
                accuracy = %report["gate"]["accuracy"],
                "conversion finished"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                target: TRACE_TARGET,
                op = "convert",
                model,
                seconds = started.elapsed().as_secs_f32(),
                permanent = e.is_permanent(),
                error = %e,
                "conversion failed"
            ),
        }
        Ok(result?)
    }
}

/// Threads scoring the gate set: half the machine's, at most [`SCORE_THREADS_MAX`].
fn score_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, SCORE_THREADS_MAX))
        .unwrap_or(1)
}

/// The `.onnx` and its report, checked against each other.
fn read_result(request: &job::RunRequest) -> Result<TaskResult, ConvertError> {
    let bytes = std::fs::read(&request.output).map_err(|e| {
        ConvertError::InvalidOutput(format!("reading {}: {e}", request.output.display()))
    })?;
    let text = std::fs::read_to_string(&request.report).map_err(|e| {
        ConvertError::InvalidOutput(format!("reading {}: {e}", request.report.display()))
    })?;
    let report: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ConvertError::InvalidOutput(format!("the report is not JSON: {e}")))?;
    check_report(&report, &bytes)?;
    Ok(TaskResult::ModelConvert {
        bytes,
        ext: "onnx".into(),
        report,
    })
}

/// The report describes these bytes and carries every part the spec names.
fn check_report(report: &serde_json::Value, bytes: &[u8]) -> Result<(), ConvertError> {
    let invalid = |message: String| Err(ConvertError::InvalidOutput(message));
    if report["kind"] != "model_convert" {
        return invalid(format!("the report's kind is {}", report["kind"]));
    }
    let sha256 = hex::encode(Sha256::digest(bytes));
    if report["sha256"] != sha256.as_str() || report["bytes"] != bytes.len() {
        return invalid(format!(
            "the report names {} ({} bytes), the file is {sha256} ({} bytes)",
            report["sha256"],
            report["bytes"],
            bytes.len()
        ));
    }
    let size = report.to_string().len();
    if size > MAX_REPORT_BYTES {
        return invalid(format!(
            "the report is {size} bytes, above the studio's {MAX_REPORT_BYTES}"
        ));
    }
    for field in [
        "converter",
        "runtime",
        "input",
        "output",
        "labels",
        "labelsMatchCategories",
        "gate",
        "references",
        "durationsMs",
    ] {
        if report[field].is_null() {
            return invalid(format!("the report has no `{field}`"));
        }
    }
    Ok(())
}

impl Engine for ConvertEngine {
    fn name(&self) -> &'static str {
        "convert"
    }

    /// Offers of `convert` models route here only where they can run.
    fn model_engines(&self) -> Vec<ModelEngine> {
        match self.platform {
            Platform::Supported => vec![ModelEngine::Convert],
            Platform::Unsupported(_) => Vec::new(),
        }
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut capabilities = EngineCapabilities::default();
        if self.platform == Platform::Supported {
            capabilities
                .supported_models_per_kind
                .insert(TaskKind::ModelConvert, vec![MODEL_SENTINEL.to_string()]);
        }
        capabilities
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        match task {
            Task::ModelConvert(params) => self.convert(model, params),
            other => Err(UnsupportedTask::new("convert", other.kind()).into()),
        }
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        _source: &ModelSource,
    ) -> Result<TaskResult> {
        self.dispatch(model, task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MeshParams;

    fn engine(platform: Platform) -> (tempfile::TempDir, ConvertEngine) {
        let root = tempfile::tempdir().unwrap();
        let engine = ConvertEngine::with_platform(root.path(), platform);
        (root, engine)
    }

    #[test]
    fn advertises_model_convert_only_where_it_runs() {
        let (_root, supported) = engine(Platform::classify(true));
        assert_eq!(supported.model_engines(), vec![ModelEngine::Convert]);
        assert!(supported
            .capabilities()
            .supports(TaskKind::ModelConvert, MODEL_SENTINEL));
        let (_root, unsupported) = engine(Platform::classify(false));
        assert!(unsupported.model_engines().is_empty());
        assert!(unsupported.capabilities().kinds().is_empty());
    }

    #[test]
    fn refuses_other_kinds_and_unsupported_platforms_permanently() {
        let (_root, supported) = engine(Platform::Supported);
        let mesh = Task::Mesh(MeshParams::default());
        let error = supported.dispatch("tf2onnx", mesh).unwrap_err();
        assert!(crate::runtime::is_permanent_failure(&error));
        let (_root, unsupported) = engine(Platform::classify(false));
        let task = Task::ModelConvert(job::tests::task());
        let error = unsupported.dispatch("tf2onnx", task).unwrap_err();
        assert!(crate::runtime::is_permanent_failure(&error));
    }

    #[test]
    fn refuses_a_bad_task_before_provisioning_anything() {
        let (root, engine) = engine(Platform::Supported);
        let mut task = job::tests::task();
        task.converter = "onnxmltools".into();
        let error = engine
            .dispatch("tf2onnx", Task::ModelConvert(task))
            .unwrap_err();
        assert!(crate::runtime::is_permanent_failure(&error), "{error:#}");
        assert!(!root.path().join("torch").exists());
    }

    #[test]
    fn a_report_must_describe_the_file() {
        let task = job::tests::task();
        let TaskResult::ModelConvert { bytes, report, .. } = synthetic::result(&task) else {
            panic!("expected a model");
        };
        check_report(&report, &bytes).unwrap();
        let error = check_report(&report, b"other bytes").unwrap_err();
        assert!(error.to_string().contains("the report names"), "{error}");
        let mut partial = report.clone();
        partial.as_object_mut().unwrap().remove("references");
        assert!(check_report(&partial, &bytes)
            .unwrap_err()
            .to_string()
            .contains("`references`"));
        let mut other = report;
        other["kind"] = "mesh".into();
        assert!(check_report(&other, &bytes).is_err());
        let mut large = other;
        large["kind"] = "model_convert".into();
        large["padding"] = "x".repeat(MAX_REPORT_BYTES).into();
        assert!(check_report(&large, &bytes)
            .unwrap_err()
            .to_string()
            .contains("above the studio"));
    }

    #[test]
    fn reads_the_converters_files_back() {
        let task = job::tests::task();
        let TaskResult::ModelConvert { bytes, report, .. } = synthetic::result(&task) else {
            panic!("expected a model");
        };
        let work = tempfile::tempdir().unwrap();
        let request = job::RunRequest {
            task,
            categories_url: String::new(),
            dataset_url: String::new(),
            output: work.path().join("model.onnx"),
            report: work.path().join("report.json"),
            gate_cache: work.path().join("gate-sets"),
            concurrency: 1,
            threads: 1,
        };
        assert!(matches!(
            read_result(&request),
            Err(ConvertError::InvalidOutput(_))
        ));
        std::fs::write(&request.output, &bytes).unwrap();
        std::fs::write(&request.report, "not json").unwrap();
        assert!(read_result(&request)
            .unwrap_err()
            .to_string()
            .contains("not JSON"));
        std::fs::write(&request.report, report.to_string()).unwrap();
        let TaskResult::ModelConvert { ext, .. } = read_result(&request).unwrap() else {
            panic!("expected a model");
        };
        assert_eq!(ext, "onnx");
    }

    #[test]
    fn scores_on_half_the_machine_at_most() {
        assert!((1..=SCORE_THREADS_MAX).contains(&score_threads()));
    }
}
