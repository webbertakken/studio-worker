//! The torch engine: PyTorch models with custom CUDA extensions, one child process per job
//! (`docs/engines/torch.md`).  Today it serves the `mesh` kind: a picture in, a GLB out, with
//! Pixal3D, TRELLIS.2 and Hunyuan3D 2.1.
//!
//! It provisions everything it runs under `models_root` (`uv`, CUDA, an environment per recipe,
//! the weights), caps each run at the model's device-memory budget and never unloads a model it
//! did not load: admission happens before a job reaches it, as for every transient job.

pub mod cuda;
pub mod error;
pub mod layout;
pub mod platform;
pub mod provision;
pub mod recipe;
pub mod runner;
pub mod stream;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};

pub use error::TorchError;

use super::{Engine, EngineCapabilities, UnsupportedTask};
use crate::types::{MeshParams, ModelEngine, ModelSource, Task, TaskKind, TaskResult};
use layout::Layout;
use platform::Platform;
use provision::{CudaSource, Provisioner, UvSource};
use recipe::Recipe;
use runner::RunRequest;

/// Tracing target of the torch engine.  Stable, so operators can filter
/// `RUST_LOG=studio_worker::engine::torch=debug`.
pub const TRACE_TARGET: &str = "studio_worker::engine::torch";

/// Triangles in a web-sized GLB (Webber, 2026-10-20: "40k triangles, 1024 px texture, about 3 to
/// 8 MB").  Used when a model's `cliDefaults` set no `decimationTarget`.
/// Safe range 10 000..=1 000 000.
pub const WEB_GLB_TRIANGLES: u32 = 40_000;
/// Texture pixels per side in a web-sized GLB (Webber, 2026-10-20).  Used when a model's
/// `cliDefaults` set no `textureSize`.  Safe range 512..=4096.
pub const WEB_GLB_TEXTURE_PX: u32 = 1024;
/// The longest a run may take before it is stopped: a Pixal3D run is about 3 minutes, Hunyuan3D
/// with CPU offload is slower.  Provisioning has its own limit per step.  Safe range 10..=55 min.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The model id the engine advertises for the `mesh` kind: it serves whatever the studio's
/// registry names, by pipeline.
pub const MODEL_SENTINEL: &str = "torch:*";

/// A model's adapter in `python/pipelines/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipeline {
    Pixal3d,
    Trellis2,
    Hunyuan3d,
}

impl Pipeline {
    pub fn parse(name: &str) -> Result<Self, TorchError> {
        match name {
            "pixal3d" => Ok(Pipeline::Pixal3d),
            "trellis2" => Ok(Pipeline::Trellis2),
            "hunyuan3d" => Ok(Pipeline::Hunyuan3d),
            other => Err(TorchError::UnknownPipeline(other.to_string())),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Pipeline::Pixal3d => "pixal3d",
            Pipeline::Trellis2 => "trellis2",
            Pipeline::Hunyuan3d => "hunyuan3d",
        }
    }

    /// The environment it runs in.
    pub fn recipe(self) -> &'static Recipe {
        match self {
            Pipeline::Pixal3d | Pipeline::Trellis2 => &recipe::TRELLIS,
            Pipeline::Hunyuan3d => &recipe::HUNYUAN3D,
        }
    }
}

/// How a model runs, read from its `cliDefaults`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub pipeline: Pipeline,
    pub budget_gib: f32,
    pub steps: u32,
    pub guidance: f32,
    pub resolution: Option<u32>,
    pub low_vram: bool,
    pub decimation_target: u32,
    pub texture_size: u32,
}

impl Settings {
    pub fn of(source: &ModelSource) -> Result<Self, TorchError> {
        let defaults = &source.cli_defaults;
        let pipeline = defaults
            .pipeline
            .as_deref()
            .ok_or(TorchError::MissingDefault { field: "pipeline" })
            .and_then(Pipeline::parse)?;
        let budget_gib =
            defaults
                .max_vram_gib
                .filter(|gib| *gib > 0.0)
                .ok_or(TorchError::MissingDefault {
                    field: "maxVramGib",
                })?;
        Ok(Self {
            pipeline,
            budget_gib,
            steps: defaults.steps,
            guidance: defaults.cfg_scale,
            resolution: defaults.resolution,
            low_vram: defaults.low_vram.unwrap_or(false),
            decimation_target: defaults.decimation_target.unwrap_or(WEB_GLB_TRIANGLES),
            texture_size: defaults.texture_size.unwrap_or(WEB_GLB_TEXTURE_PX),
        })
    }
}

pub struct TorchEngine {
    platform: Platform,
    layout: Layout,
    uv: UvSource,
    cuda: CudaSource,
    run_timeout: Duration,
}

impl TorchEngine {
    /// The engine for this machine: probes the GPU once.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn new(models_root: &Path) -> Self {
        Self::with_platform(models_root, platform::probe())
    }

    /// The engine for a known platform, with the pinned `uv` and CUDA sources.
    pub fn with_platform(models_root: &Path, platform: Platform) -> Self {
        Self {
            platform,
            layout: Layout::new(models_root),
            uv: UvSource::from_env(),
            cuda: CudaSource::default(),
            run_timeout: RUN_TIMEOUT,
        }
    }

    /// Replace where `uv` and CUDA come from, and the run limit (tests).
    pub fn with_sources(mut self, uv: UvSource, cuda: CudaSource, run_timeout: Duration) -> Self {
        self.uv = uv;
        self.cuda = cuda;
        self.run_timeout = run_timeout;
        self
    }

    fn mesh(&self, model: &str, params: MeshParams, source: &ModelSource) -> Result<TaskResult> {
        let Some(capability) = self.platform.capability() else {
            return Err(UnsupportedTask::new("torch", TaskKind::Mesh).into());
        };
        let settings = Settings::of(source)?;
        let started = Instant::now();
        let work = tempfile::Builder::new()
            .prefix("studio-worker-mesh-")
            .tempdir()
            .context("creating the job's temporary directory")?;
        let picture = crate::vision::fetch(&params.image_url)
            .map_err(|e| TorchError::BadInput(format!("{e:#}")))?;
        let extension = crate::engine::download::sniff_image_extension(&picture)
            .ok_or_else(|| TorchError::BadInput("not a PNG, JPEG or WebP picture".into()))?;
        image::load_from_memory(&picture)
            .map_err(|e| TorchError::BadInput(format!("the picture cannot be decoded: {e}")))?;
        let image = work.path().join(format!("input.{extension}"));
        std::fs::write(&image, &picture).context("writing the picture")?;
        let seed = params
            .seed
            .unwrap_or_else(|| u64::from(getrandom::u32().unwrap_or(42)));
        tracing::info!(
            target: TRACE_TARGET,
            op = "run",
            model,
            pipeline = settings.pipeline.name(),
            seed,
            budget_gib = settings.budget_gib,
            "mesh job started"
        );
        let provisioner = Provisioner {
            layout: self.layout.clone(),
            capability,
            uv: self.uv.clone(),
            cuda: self.cuda.clone(),
        };
        let environment = provisioner.ensure(settings.pipeline.recipe())?;
        let output = work.path().join("model.glb");
        let request = RunRequest {
            pipeline: settings.pipeline.name().into(),
            image,
            output: output.clone(),
            work_dir: work.path().to_path_buf(),
            seed,
            steps: settings.steps,
            guidance: settings.guidance,
            resolution: settings.resolution,
            low_vram: settings.low_vram,
            decimation_target: settings.decimation_target,
            texture_size: settings.texture_size,
            budget_gib: settings.budget_gib,
            sources: environment
                .sources
                .iter()
                .cloned()
                .collect::<BTreeMap<_, _>>(),
            pipelines_dir: self.layout.pipelines(),
        };
        let report = runner::run(&provisioner, &environment, &request, self.run_timeout)
            .inspect_err(|e| {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "run",
                    model,
                    pipeline = settings.pipeline.name(),
                    seconds = started.elapsed().as_secs_f32(),
                    permanent = e.is_permanent(),
                    error = %e,
                    "mesh job failed"
                )
            })?;
        let bytes = std::fs::read(&output)
            .map_err(|e| TorchError::InvalidGlb(format!("reading {}: {e}", output.display())))?;
        crate::glb::check(&bytes).map_err(|e| TorchError::InvalidGlb(e.to_string()))?;
        tracing::info!(
            target: TRACE_TARGET,
            op = "run",
            model,
            pipeline = settings.pipeline.name(),
            seconds = started.elapsed().as_secs_f32(),
            run_seconds = report.seconds,
            triangles = report.triangles,
            bytes = bytes.len(),
            vram_peak_gib = report.vram_peak_gib,
            "mesh job finished"
        );
        Ok(TaskResult::Mesh {
            bytes,
            ext: "glb".into(),
        })
    }
}

impl Engine for TorchEngine {
    fn name(&self) -> &'static str {
        "torch"
    }

    /// Offers of `torch` models route here only where they can run.
    fn model_engines(&self) -> Vec<ModelEngine> {
        match self.platform {
            Platform::Supported(_) => vec![ModelEngine::Torch],
            Platform::Unsupported(_) => Vec::new(),
        }
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut capabilities = EngineCapabilities::default();
        if let Platform::Supported(_) = self.platform {
            capabilities
                .supported_models_per_kind
                .insert(TaskKind::Mesh, vec![MODEL_SENTINEL.to_string()]);
        }
        capabilities
    }

    fn dispatch(&self, model: &str, _task: Task) -> Result<TaskResult> {
        bail!("the torch engine requires a ModelSource on the offer (model `{model}`)")
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        match task {
            Task::Mesh(params) => self.mesh(model, params, source),
            other => Err(UnsupportedTask::new("torch", other.kind()).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCliDefaults, ModelEngine};
    use platform::ComputeCapability;

    const ADA: Platform = Platform::Supported(ComputeCapability { major: 8, minor: 9 });

    fn source(pipeline: Option<&str>, budget: Option<f32>) -> ModelSource {
        ModelSource {
            engine: ModelEngine::Torch,
            files: vec![],
            cli_defaults: ModelCliDefaults {
                cfg_scale: 7.5,
                steps: 12,
                width: 1024,
                height: 1024,
                pipeline: pipeline.map(str::to_string),
                max_vram_gib: budget,
                ..Default::default()
            },
        }
    }

    fn mesh_task(url: &str) -> Task {
        Task::Mesh(MeshParams {
            image_url: url.into(),
            seed: Some(1),
            ext: "glb".into(),
        })
    }

    #[test]
    fn settings_read_the_cli_defaults_with_web_sized_defaults() {
        let settings = Settings::of(&source(Some("pixal3d"), Some(16.5))).unwrap();
        assert_eq!(settings.pipeline, Pipeline::Pixal3d);
        assert_eq!(settings.budget_gib, 16.5);
        assert_eq!(settings.decimation_target, WEB_GLB_TRIANGLES);
        assert_eq!(settings.texture_size, WEB_GLB_TEXTURE_PX);
        assert_eq!((settings.steps, settings.guidance), (12, 7.5));
        assert!(!settings.low_vram);
        let mut own = source(Some("hunyuan3d"), Some(16.0));
        own.cli_defaults.decimation_target = Some(100_000);
        own.cli_defaults.texture_size = Some(2048);
        own.cli_defaults.low_vram = Some(true);
        own.cli_defaults.resolution = Some(512);
        let settings = Settings::of(&own).unwrap();
        assert_eq!(
            (
                settings.decimation_target,
                settings.texture_size,
                settings.low_vram,
                settings.resolution
            ),
            (100_000, 2048, true, Some(512))
        );
        assert_eq!(settings.pipeline.recipe().name, "hunyuan3d");
    }

    #[test]
    fn a_model_without_pipeline_or_budget_is_refused_for_good() {
        let missing = |s: ModelSource| Settings::of(&s).unwrap_err();
        assert!(matches!(
            missing(source(None, Some(16.0))),
            TorchError::MissingDefault { field: "pipeline" }
        ));
        assert!(matches!(
            missing(source(Some("pixal3d"), None)),
            TorchError::MissingDefault {
                field: "maxVramGib"
            }
        ));
        assert!(matches!(
            missing(source(Some("pixal3d"), Some(0.0))),
            TorchError::MissingDefault {
                field: "maxVramGib"
            }
        ));
        let unknown = missing(source(Some("sf3d"), Some(8.0)));
        assert!(unknown.is_permanent());
        assert!(unknown.to_string().contains("sf3d"));
    }

    #[test]
    fn pipelines_share_the_trellis_environment_except_hunyuan() {
        for (name, environment) in [
            ("pixal3d", "trellis"),
            ("trellis2", "trellis"),
            ("hunyuan3d", "hunyuan3d"),
        ] {
            let pipeline = Pipeline::parse(name).unwrap();
            assert_eq!(pipeline.name(), name);
            assert_eq!(pipeline.recipe().name, environment);
        }
    }

    #[test]
    fn mesh_is_advertised_only_where_it_can_run() {
        let dir = tempfile::tempdir().unwrap();
        let here = TorchEngine::with_platform(dir.path(), ADA);
        assert!(here.capabilities().supports(TaskKind::Mesh, MODEL_SENTINEL));
        assert_eq!(here.model_engines(), vec![ModelEngine::Torch]);
        let elsewhere =
            TorchEngine::with_platform(dir.path(), Platform::Unsupported("no GPU".into()));
        assert!(elsewhere
            .capabilities()
            .supported_models_per_kind
            .is_empty());
        assert!(elsewhere.model_engines().is_empty());
        let err = elsewhere
            .dispatch_with_source(
                "pixal3d",
                mesh_task("data:,"),
                &source(Some("pixal3d"), Some(16.0)),
            )
            .unwrap_err();
        assert!(crate::runtime::is_permanent_failure(&err), "{err}");
    }

    #[test]
    fn other_kinds_and_sourceless_jobs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let engine = TorchEngine::with_platform(dir.path(), ADA);
        let image = Task::Image(Default::default());
        let err = engine
            .dispatch_with_source("x", image.clone(), &source(Some("pixal3d"), Some(16.0)))
            .unwrap_err();
        assert!(crate::runtime::is_permanent_failure(&err), "{err}");
        assert!(engine
            .dispatch("x", image)
            .unwrap_err()
            .to_string()
            .contains("ModelSource"));
    }

    #[test]
    fn a_picture_that_cannot_be_read_fails_for_good_before_provisioning() {
        let dir = tempfile::tempdir().unwrap();
        let engine = TorchEngine::with_platform(dir.path(), ADA);
        for url in [
            "data:image/png;base64,AAAA",
            "data:image/png;base64,aGVsbG8gd29ybGQ=",
        ] {
            let err = engine
                .dispatch_with_source(
                    "pixal3d",
                    mesh_task(url),
                    &source(Some("pixal3d"), Some(16.0)),
                )
                .unwrap_err();
            assert!(crate::runtime::is_permanent_failure(&err), "{err:#}");
            assert!(err.to_string().contains("picture"), "{err:#}");
        }
        assert!(
            !dir.path().join("torch").exists(),
            "nothing was provisioned"
        );
    }
}
