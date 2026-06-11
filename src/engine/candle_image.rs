//! Real image generation via [`candle-transformers`] Stable Diffusion.
//!
//! Compiled in with `--features image-candle`.  Runs SD v1.5 on CPU
//! (or whatever candle backend the user's build supports).  Models are
//! downloaded via `hf-hub` on first use into the standard HF cache —
//! they don't live in `<models_root>` because candle's loaders expect
//! the HF directory layout.
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{anyhow, bail, Context, Result};
use candle_core::{DType, Device, IndexOp, Module, Tensor};
use candle_transformers::models::stable_diffusion;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokenizers::Tokenizer;
use tracing::{debug, info, warn};

/// Tracing target for the candle image engine.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::engine::candle_image=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::candle_image";

const HF_REPO_V1_5: &str = "stable-diffusion-v1-5/stable-diffusion-v1-5";
const HF_TOKENIZER: &str = "openai/clip-vit-base-patch32";
const MODEL_ID: &str = "stable-diffusion-v1-5";

pub struct CandleImageEngine {
    cached: Mutex<Option<CachedModel>>,
}

struct CachedModel {
    id: String,
    pipeline: Arc<Pipeline>,
}

struct Pipeline {
    device: Device,
    dtype: DType,
    sd_config: stable_diffusion::StableDiffusionConfig,
    tokenizer: Tokenizer,
    pad_id: u32,
    text_model: stable_diffusion::clip::ClipTextTransformer,
    unet: stable_diffusion::unet_2d::UNet2DConditionModel,
    vae: stable_diffusion::vae::AutoEncoderKL,
    vae_scale: f64,
}

impl CandleImageEngine {
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    fn build_pipeline(&self, width: usize, height: usize) -> Result<Pipeline> {
        use hf_hub::api::sync::Api;
        info!(
            target: TRACE_TARGET,
            op = "build_pipeline",
            model = MODEL_ID,
            width,
            height,
            "building SD pipeline (may download weights)"
        );
        let build_started = Instant::now();
        let api = Api::new().context("creating hf-hub api")?;

        let tokenizer_path = download_with_trace(&api, HF_TOKENIZER, "tokenizer.json")
            .context("downloading clip tokenizer")?;
        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow!("loading tokenizer: {e}"))?;
        let pad_id = tokenizer
            .get_vocab(true)
            .get("<|endoftext|>")
            .copied()
            .ok_or_else(|| anyhow!("clip tokenizer missing <|endoftext|>"))?;

        let sd_config =
            stable_diffusion::StableDiffusionConfig::v1_5(None, Some(height), Some(width));

        let clip_weights =
            download_with_trace(&api, HF_REPO_V1_5, "text_encoder/model.safetensors")
                .context("downloading clip weights")?;
        let unet_weights = download_with_trace(
            &api,
            HF_REPO_V1_5,
            "unet/diffusion_pytorch_model.safetensors",
        )
        .context("downloading unet weights")?;
        let vae_weights = download_with_trace(
            &api,
            HF_REPO_V1_5,
            "vae/diffusion_pytorch_model.safetensors",
        )
        .context("downloading vae weights")?;

        let device = Device::Cpu;
        let dtype = DType::F32;

        let text_model = stable_diffusion::build_clip_transformer(
            &sd_config.clip,
            clip_weights,
            &device,
            dtype,
        )?;
        let unet = sd_config.build_unet(unet_weights, &device, 4, false, dtype)?;
        let vae = sd_config.build_vae(vae_weights, &device, dtype)?;
        info!(
            target: TRACE_TARGET,
            op = "build_pipeline",
            model = MODEL_ID,
            elapsed_ms = build_started.elapsed().as_millis() as u64,
            "SD pipeline ready"
        );

        Ok(Pipeline {
            device,
            dtype,
            sd_config,
            tokenizer,
            pad_id,
            text_model,
            unet,
            vae,
            vae_scale: 0.18215,
        })
    }

    fn load_or_get(&self, model: &str, width: usize, height: usize) -> Result<Arc<Pipeline>> {
        let mut guard = self.cached.lock();
        if let Some(c) = &*guard {
            if c.id == model {
                debug!(
                    target: TRACE_TARGET,
                    op = "load",
                    model,
                    cache = "hit",
                    "reusing cached pipeline"
                );
                return Ok(c.pipeline.clone());
            }
        }
        let pipeline = Arc::new(self.build_pipeline(width, height).inspect_err(|e| {
            warn!(
                target: TRACE_TARGET,
                op = "load",
                model,
                error = %e,
                "failed to build pipeline"
            );
        })?);
        *guard = Some(CachedModel {
            id: model.to_string(),
            pipeline: pipeline.clone(),
        });
        Ok(pipeline)
    }
}

fn download_with_trace(
    api: &hf_hub::api::sync::Api,
    repo: &str,
    file: &str,
) -> Result<PathBuf, hf_hub::api::sync::ApiError> {
    debug!(
        target: TRACE_TARGET,
        op = "download",
        repo,
        file,
        "requesting weight file"
    );
    let started = Instant::now();
    let result = api.model(repo.to_string()).get(file);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(path) => debug!(
            target: TRACE_TARGET,
            op = "download",
            repo,
            file,
            path = %path.display(),
            elapsed_ms,
            "weight file ready"
        ),
        Err(e) => warn!(
            target: TRACE_TARGET,
            op = "download",
            repo,
            file,
            elapsed_ms,
            error = %e,
            "weight download failed"
        ),
    }
    result
}

impl Default for CandleImageEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn encode_text(pipeline: &Pipeline, prompt: &str) -> Result<Tensor> {
    let max_pos = pipeline.sd_config.clip.max_position_embeddings;
    let mut ids = pipeline
        .tokenizer
        .encode(prompt, true)
        .map_err(|e| anyhow!("tokenize: {e}"))?
        .get_ids()
        .to_vec();
    if ids.len() > max_pos {
        ids.truncate(max_pos);
    }
    while ids.len() < max_pos {
        ids.push(pipeline.pad_id);
    }
    let tokens = Tensor::new(ids.as_slice(), &pipeline.device)?.unsqueeze(0)?;
    let cond = pipeline.text_model.forward(&tokens)?;

    // Unconditional embeddings (empty prompt) for classifier-free guidance.
    let mut uncond_ids = pipeline
        .tokenizer
        .encode("", true)
        .map_err(|e| anyhow!("tokenize uncond: {e}"))?
        .get_ids()
        .to_vec();
    while uncond_ids.len() < max_pos {
        uncond_ids.push(pipeline.pad_id);
    }
    let uncond_tokens = Tensor::new(uncond_ids.as_slice(), &pipeline.device)?.unsqueeze(0)?;
    let uncond = pipeline.text_model.forward(&uncond_tokens)?;

    Tensor::cat(&[uncond, cond], 0)?
        .to_dtype(pipeline.dtype)
        .map_err(Into::into)
}

fn run_diffusion(
    pipeline: &Pipeline,
    text_embeddings: &Tensor,
    n_steps: usize,
    guidance_scale: f64,
    seed: u64,
) -> Result<Tensor> {
    pipeline.device.set_seed(seed)?;
    let mut scheduler = pipeline.sd_config.build_scheduler(n_steps)?;
    let mut latents = Tensor::randn(
        0f32,
        1f32,
        (
            1,
            4,
            pipeline.sd_config.height / 8,
            pipeline.sd_config.width / 8,
        ),
        &pipeline.device,
    )?;
    latents = (latents * scheduler.init_noise_sigma())?;
    latents = latents.to_dtype(pipeline.dtype)?;
    let timesteps = scheduler.timesteps().to_vec();

    for &timestep in &timesteps {
        let latent_model_input = Tensor::cat(&[&latents, &latents], 0)?;
        let latent_model_input = scheduler.scale_model_input(latent_model_input, timestep)?;
        let noise_pred =
            pipeline
                .unet
                .forward(&latent_model_input, timestep as f64, text_embeddings)?;
        let noise_pred = noise_pred.chunk(2, 0)?;
        let noise_pred = (&noise_pred[0] + ((&noise_pred[1] - &noise_pred[0])? * guidance_scale)?)?;
        latents = scheduler.step(&noise_pred, timestep, &latents)?;
    }
    Ok(latents)
}

fn decode_to_png(pipeline: &Pipeline, latents: &Tensor) -> Result<Vec<u8>> {
    let images = pipeline.vae.decode(&(latents / pipeline.vae_scale)?)?;
    let images = ((images / 2.0)? + 0.5)?.to_device(&Device::Cpu)?;
    let images = (images.clamp(0f32, 1.0)? * 255.0)?.to_dtype(DType::U8)?;
    let image = images.i(0)?; // [3, H, W]
    let (channels, height, width) = image.dims3()?;
    if channels != 3 {
        bail!("expected 3-channel image, got {channels}");
    }
    let raw = image.permute((1, 2, 0))?.flatten_all()?.to_vec1::<u8>()?;
    let buffer =
        image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width as u32, height as u32, raw)
            .ok_or_else(|| anyhow!("RGB buffer wrong size"))?;
    let dyn_img = image::DynamicImage::ImageRgb8(buffer);
    let mut out = Cursor::new(Vec::<u8>::new());
    dyn_img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

impl Engine for CandleImageEngine {
    fn name(&self) -> &'static str {
        "image-candle"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Image, vec![MODEL_ID.to_string()]);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let started = Instant::now();
        if model != MODEL_ID {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                model,
                expected = MODEL_ID,
                "unsupported model id"
            );
            bail!("candle-image engine only serves `{MODEL_ID}`, got `{model}`");
        }
        let params = match task {
            Task::Image(p) => p,
            other => {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    kind = kind.as_str(),
                    model,
                    "unsupported task kind"
                );
                return Err(
                    crate::engine::UnsupportedTask::new("candle-image", other.kind()).into(),
                );
            }
        };
        let width = (params.width as usize).max(64);
        let height = (params.height as usize).max(64);
        // Dimensions must be multiples of 64 for SD's UNet.
        let width = width - (width % 64);
        let height = height - (height % 64);
        let pipeline = self.load_or_get(model, width, height)?;
        let n_steps = params.steps.max(1) as usize;
        let seed = params.seed.unwrap_or(0);
        debug!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = kind.as_str(),
            model,
            width,
            height,
            steps = n_steps,
            seed,
            "starting diffusion"
        );
        let text_embeddings = encode_text(&pipeline, &params.prompt)?;
        let latents =
            run_diffusion(&pipeline, &text_embeddings, n_steps, 7.5, seed).inspect_err(|e| {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    kind = kind.as_str(),
                    model,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %e,
                    "diffusion failed"
                );
            })?;
        let png = decode_to_png(&pipeline, &latents)?;
        info!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = kind.as_str(),
            model,
            bytes = png.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "image generated"
        );
        Ok(TaskResult::Image {
            bytes: png,
            ext: "png".into(),
        })
    }
}

/// Where hf-hub will cache downloaded model weights.  Reported for
/// diagnostics only — the engine doesn't manage this directory itself.
pub fn hf_cache_path() -> Option<PathBuf> {
    let api = hf_hub::api::sync::Api::new().ok()?;
    let path = api
        .model(HF_REPO_V1_5.to_string())
        .get("model_index.json")
        .ok()?;
    path.parent().map(|x| x.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_advertises_image_kind() {
        let engine = CandleImageEngine::new();
        let caps = engine.capabilities();
        assert_eq!(
            caps.supported_models_per_kind[&TaskKind::Image],
            vec![MODEL_ID.to_string()]
        );
        assert_eq!(engine.name(), "image-candle");
    }

    #[test]
    fn dispatch_rejects_wrong_model_id() {
        let engine = CandleImageEngine::new();
        let task = Task::Image(ImageParams {
            prompt: "x".into(),
            width: 64,
            height: 64,
            steps: 1,
            seed: None,
            ext: "png".into(),
            ..Default::default()
        });
        let err = engine.dispatch("not-the-model", task).unwrap_err();
        assert!(err.to_string().contains("only serves"));
    }

    #[test]
    fn dispatch_rejects_non_image_tasks() {
        let engine = CandleImageEngine::new();
        let task = Task::Llm(LlmParams {
            messages: vec![],
            max_tokens: 1,
            temperature: 0.0,
            ..Default::default()
        });
        let err = engine.dispatch(MODEL_ID, task).unwrap_err();
        assert!(err.to_string().contains("cannot serve llm"));
    }
}
