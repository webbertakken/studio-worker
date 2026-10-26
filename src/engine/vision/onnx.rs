//! Vision models on ONNX Runtime: the process-wide runtime (`onnx_provision`), on CUDA when
//! the host has a complete CUDA runtime, else on the CPU.

use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::{Tensor, TensorElementType};
use parking_lot::Mutex;

use super::postprocess::RawTensor;
use super::{VisionModel, VisionSpec};
use crate::catalog::CatalogModel;
use crate::engine::{download, onnx_provision};
use crate::host::LoadedModel;
use crate::types::ModelFileRole;

const TRACE_TARGET: &str = "studio_worker::engine::vision";

/// A vision network in memory: one session, one request at a time (`run` needs `&mut`).
pub struct OnnxVision {
    spec: VisionSpec,
    device: &'static str,
    input: String,
    output: String,
    session: Mutex<Session>,
}

impl LoadedModel for OnnxVision {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_vision(&self) -> Option<&dyn VisionModel> {
        Some(self)
    }
}

impl VisionModel for OnnxVision {
    fn spec(&self) -> &VisionSpec {
        &self.spec
    }

    fn device(&self) -> &'static str {
        self.device
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run(&self, input: Vec<f32>) -> Result<RawTensor> {
        let tensor = Tensor::from_array((self.spec.input_shape(), input))
            .context("building the input tensor")?;
        let mut session = self.session.lock();
        let outputs = session
            .run(ort::inputs![self.input.as_str() => tensor])
            .with_context(|| format!("running {}", self.spec.model))?;
        let (shape, data) = outputs[self.output.as_str()]
            .try_extract_tensor::<f32>()
            .with_context(|| format!("reading output {} of {}", self.output, self.spec.model))?;
        Ok(RawTensor {
            shape: shape.to_vec(),
            data: data.to_vec(),
        })
    }
}

/// Download (if needed) and load `model`.  Dropping the result frees it.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn load(models_root: &Path, model: &CatalogModel) -> Result<OnnxVision> {
    let started = Instant::now();
    let runtime = onnx_provision::ensure_runtime(models_root)?;
    // A same-named file in the models root is reused only at its declared size: names such as
    // `config.json` are common to many models.
    let dir = download::model_dir(models_root, &model.id);
    let file = |role: ModelFileRole| model.source.files.iter().find(|f| f.role == role);
    let weights = file(ModelFileRole::Model)
        .ok_or_else(|| anyhow!("model {} has no `model` file (the .onnx)", model.id))?;
    let weights = download::ensure_file_reusing(&dir, models_root, weights)
        .with_context(|| format!("downloading the weights of {}", model.id))?;
    let labels = match file(ModelFileRole::Labels) {
        Some(labels) => {
            let path = download::ensure_file_reusing(&dir, models_root, labels)
                .with_context(|| format!("downloading the labels of {}", model.id))?;
            Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )
        }
        None => None,
    };
    let spec = VisionSpec::from_model(model, labels.as_deref())?;

    let mut builder = Session::builder(&runtime.environment)?;
    if runtime.flavour.is_cuda() {
        // Strict: a CUDA provider that cannot load fails the load instead of running on the
        // CPU unnoticed.
        builder = builder
            .with_execution_providers([ort::ep::CUDA::default().build().error_on_failure()])
            .map_err(ort::Error::<()>::from)?;
    }
    let session = builder
        .commit_from_file(&weights)
        .with_context(|| format!("loading {}", weights.display()))?;
    let (input, output) = check_io(&session, &spec)?;
    let device = if runtime.flavour.is_cuda() {
        "cuda"
    } else {
        "cpu"
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "load",
        model = %model.id,
        task = spec.task.as_str(),
        device,
        flavour = runtime.flavour.name(),
        input = %input,
        output = %output,
        labels = spec.labels.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "vision model loaded"
    );
    Ok(OnnxVision {
        spec,
        device,
        input,
        output,
        session: Mutex::new(session),
    })
}

/// The names of the session's single image input and its first output, refused when the
/// input takes no f32 tensor of the spec's shape, or a classifier's output holds no score per
/// label.
#[cfg_attr(coverage_nightly, coverage(off))]
fn check_io(session: &Session, spec: &VisionSpec) -> Result<(String, String)> {
    let [input] = session.inputs() else {
        bail!(
            "model {} takes {} inputs; a vision model takes one picture",
            spec.model,
            session.inputs().len()
        );
    };
    let Some(output) = session.outputs().first() else {
        bail!("model {} has no output", spec.model);
    };
    let dtype = input.dtype();
    if dtype.tensor_type() != Some(TensorElementType::Float32) {
        bail!(
            "model {} input {} is {dtype:?}; only f32 inputs are served",
            spec.model,
            input.name()
        );
    }
    if let Some(shape) = dtype.tensor_shape() {
        let want = spec.input_shape();
        if !super::shape_fits(shape, &want) {
            bail!(
                "model {} input {} has shape {:?}; its cliDefaults ask for {want:?}",
                spec.model,
                input.name(),
                &shape[..]
            );
        }
    }
    if spec.task == crate::types::VisionTask::Classify {
        if let Some(shape) = output.dtype().tensor_shape() {
            if !super::scores_fit(shape, spec.labels.len()) {
                bail!(
                    "model {} output {} has shape {:?}; its labels name {} classes",
                    spec.model,
                    output.name(),
                    &shape[..],
                    spec.labels.len()
                );
            }
        }
    }
    Ok((input.name().to_string(), output.name().to_string()))
}
