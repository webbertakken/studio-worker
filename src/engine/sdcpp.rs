//! Engine that runs real image inference by subprocess-invoking the
//! `stable-diffusion.cpp` (`sd-cli`) binary.
//!
//! The studio's offer carries a [`ModelSource`] with everything we
//! need: an engine identifier (`sd-cpp`), the list of files to
//! download (diffusion-model + text-encoder + VAE, each with a public
//! URL + filename), and CLI defaults (cfg-scale, steps, dimensions).
//! The worker has zero hardcoded model knowledge — it caches
//! whatever the studio asks for under `cfg.models_root` and invokes
//! `sd-cli` with the files arranged by role.
//!
//! Layout under `cfg.models_root` (default `~/models`):
//! ```text
//! ~/models/<filename1>
//! ~/models/<filename2>
//! …
//! ```
//! Files are downloaded on first use - skipped when already present
//! under `cfg.models_root`.  The streamed body is checked against the
//! server's `Content-Length` so a truncated download is rejected and
//! cleaned up instead of being renamed into place as a corrupt model
//! that every later job would fail to load.  Cached files are re-used
//! across every subsequent job that names them.
//!
//! The engine self-registers only when `sd-cli` is present on the box
//! (either at `$STUDIO_WORKER_SD_CLI`, or `~/.local/bin/sd-cli`, or on
//! `$PATH`).  Without `sd-cli` the worker can't run real-image jobs
//! at all so it skips registration and the multi engine falls through
//! to synthetic for any kind it doesn't have a real backend for.

use crate::engine::download::{self, TempFileGuard};
use crate::engine::sd_provision;
use crate::engine::{Engine, EngineCapabilities};
use crate::types::{ImageParams, ModelFileRole, ModelSource, Task, TaskKind, TaskResult};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tracing::{debug, info, warn};

const TRACE_TARGET: &str = "studio_worker::engine::sdcpp";

/// Default sample-steps when the studio's `ImageParams.steps` is the
/// upstream default (20).  Z-Image-Turbo is an 8-step distilled
/// schedule so 20 wastes time; we honour `ModelSource.cliDefaults.steps`
/// instead.  Only used as the very last fallback.
const STEPS_FALLBACK: u32 = 8;

/// Worker-side engine that drives `sd-cli` per job.
///
/// `sd-cli` is resolved lazily on the first image job and cached: an
/// operator install (env / PATH / `~/.local/bin`) wins, otherwise the
/// binary is auto-provisioned into `<models_root>/bin/`.  The `Mutex`
/// serialises that one-time resolution so two concurrent jobs can't
/// race the download.
pub struct SdCppEngine {
    sd_cli: Mutex<Option<PathBuf>>,
    models_root: PathBuf,
}

impl SdCppEngine {
    /// Build the engine.  Always registers: `sd-cli` is resolved (and
    /// provisioned into `<models_root>/bin/` if missing) lazily on the
    /// first image job, so the engine serves real image work even on a
    /// box that has never had a stable-diffusion.cpp build installed.
    /// `models_root` is created on demand by the provisioner / model
    /// downloader, so registration touches no filesystem.
    pub fn new(models_root: &Path) -> Self {
        info!(
            target: TRACE_TARGET,
            op = "register",
            models_root = %models_root.display(),
            sd_cli_name = sd_provision::binary_name(),
            "sdcpp engine registered (sd-cli resolved/provisioned on first image job)"
        );
        Self {
            sd_cli: Mutex::new(None),
            models_root: models_root.to_path_buf(),
        }
    }

    /// For tests: build with explicit paths (bypasses sd-cli lookup +
    /// provisioning by seeding the resolved-path cache).
    #[cfg(test)]
    pub fn with_paths(sd_cli: PathBuf, models_root: PathBuf) -> Self {
        Self {
            sd_cli: Mutex::new(Some(sd_cli)),
            models_root,
        }
    }

    /// Resolve the `sd-cli` binary, provisioning it on first use.
    /// Resolution order (operator installs win): a cached path from a
    /// previous job, then env / `<models_root>/bin` / `~/.local/bin` /
    /// `$PATH`, then an auto-provisioned download into
    /// `<models_root>/bin/`.  The result is cached for the worker's
    /// lifetime.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_sd_cli(&self) -> Result<PathBuf> {
        let mut guard = self.sd_cli.lock();
        if let Some(p) = guard.as_ref() {
            if p.is_file() {
                return Ok(p.clone());
            }
        }
        let resolved = match resolve_sd_cli(&self.models_root) {
            Some(p) => {
                info!(
                    target: TRACE_TARGET,
                    op = "resolve",
                    sd_cli = %p.display(),
                    "using existing sd-cli"
                );
                p
            }
            None => sd_provision::provision(&self.models_root)
                .context("auto-provisioning sd-cli (stable-diffusion.cpp)")?,
        };
        *guard = Some(resolved.clone());
        Ok(resolved)
    }

    /// Ensure each file in `source.files` is present under
    /// `self.models_root`.  Downloads anything missing.  Returns the
    /// resolved local path for each file (in the same order).
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_files(&self, source: &ModelSource) -> Result<Vec<(ModelFileRole, PathBuf)>> {
        let mut out = Vec::with_capacity(source.files.len());
        for file in &source.files {
            let local = download::ensure_file(&self.models_root, file)?;
            out.push((file.role, local));
        }
        Ok(out)
    }

    /// Subprocess to `sd-cli` with the resolved diffusion / VAE /
    /// text-encoder files.  Excluded from coverage: requires an
    /// actual `sd-cli` binary + cached model files on disk, neither
    /// of which exists on the CI runner.  Exercised end-to-end via
    /// the live dev loop.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn dispatch_image(
        &self,
        model: &str,
        params: ImageParams,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        // Resolve (provisioning on first use) the sd-cli binary before
        // we touch model files, so a missing binary fails fast with the
        // provisioning error rather than after a multi-GB weight pull.
        let sd_cli = self.ensure_sd_cli()?;
        // Preflight the GPU runtime next: a missing Vulkan loader can't be
        // auto-provisioned (it ships with the driver / a system package),
        // so surface the actionable remedy now instead of after a
        // multi-GB weight pull and a cryptic sd-cli crash.
        if let Err(e) = sd_provision::vulkan_runtime_status() {
            warn!(
                target: TRACE_TARGET,
                op = "preflight",
                model,
                error = %e,
                "GPU runtime missing; refusing image job"
            );
            return Err(e);
        }
        let files = self.ensure_files(source)?;
        // A `diffusion-model` file is the standalone diffusion weights (sd-cli `--diffusion-model`,
        // used with split vae/clip); a `model` file is a full checkpoint (sd-cli `-m`/`--model`).
        // Prefer the explicit diffusion-model role; fall back to a full checkpoint.
        let diffusion_only = file_for_role(&files, ModelFileRole::DiffusionModel);
        let full_checkpoint = diffusion_only.is_none();
        let diffusion_model = diffusion_only
            .or_else(|| file_for_role(&files, ModelFileRole::Model))
            .ok_or_else(|| anyhow!("modelSource has no diffusion-model / model file"))?;
        let vae = file_for_role(&files, ModelFileRole::Vae);
        let text_encoder = file_for_role(&files, ModelFileRole::TextEncoder);
        let text_encoder_vision = file_for_role(&files, ModelFileRole::TextEncoderVision);

        let out_dir = std::env::temp_dir().join("studio-worker-sdcpp");
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating sdcpp output dir {}", out_dir.display()))?;
        let stem = format!(
            "out-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let out_path = out_dir.join(format!("{stem}.webp"));

        // Own the scratch files from the moment their paths exist so
        // every failure path (sd-cli error, unreadable output) cleans
        // up instead of leaking them into the temp dir.
        let mut temp_files = TempFileGuard::new();
        temp_files.push(out_path.clone());

        // If the task carries an init image URL, stream it to a
        // tempfile so we can hand the path to `sd-cli --init-img`.
        // This is mandatory — the worker refuses i2i jobs whose
        // init image fails to download (no silent fallback to t2i).
        // The local extension first mirrors the URL's, then is corrected
        // to the file's real content format — studio asset URLs lie
        // (`latest.webp` is often JPEG bytes) and sd-cli picks its image
        // decoder purely from the extension.
        let init_img_path = match params.init_image_url.as_deref() {
            Some(url) if !url.is_empty() => {
                let ext = init_image_extension(url);
                let init_path = out_dir.join(format!("{stem}-init.{ext}"));
                download::download_file(url, &init_path).with_context(|| {
                    format!("downloading init image {} -> {}", url, init_path.display())
                })?;
                temp_files.push(init_path.clone());
                let usable = download::ensure_correct_image_extension(&init_path)?;
                if usable != init_path {
                    temp_files.push(usable.clone());
                }
                Some(usable)
            }
            _ => None,
        };

        // A mask constrains the edit region — valid alongside either an init image (img2img
        // inpaint) or a reference image (instruction edit). Download it whenever a base image is
        // present and a mask URL was supplied; white pixels mark the region the model may change.
        let has_base = init_img_path.is_some() || params.ref_image_url.as_deref().is_some();
        let mask_path = match (has_base, params.mask_url.as_deref()) {
            (true, Some(url)) if !url.is_empty() => {
                let ext = init_image_extension(url);
                let path = out_dir.join(format!("{stem}-mask.{ext}"));
                download::download_file(url, &path)
                    .with_context(|| format!("downloading mask {} -> {}", url, path.display()))?;
                temp_files.push(path.clone());
                let usable = download::ensure_correct_image_extension(&path)?;
                if usable != path {
                    temp_files.push(usable.clone());
                }
                Some(usable)
            }
            _ => None,
        };

        // Reference image for instruction-edit models (`sd-cli -r`). Downloaded like the init image;
        // when present the arg builder uses reference mode instead of the img2img/mask path.
        let ref_img_path = match params.ref_image_url.as_deref() {
            Some(url) if !url.is_empty() => {
                let ext = init_image_extension(url);
                let path = out_dir.join(format!("{stem}-ref.{ext}"));
                download::download_file(url, &path).with_context(|| {
                    format!("downloading reference image {} -> {}", url, path.display())
                })?;
                temp_files.push(path.clone());
                let usable = download::ensure_correct_image_extension(&path)?;
                if usable != path {
                    temp_files.push(usable.clone());
                }
                Some(usable)
            }
            _ => None,
        };

        let args = build_sdcli_args(
            &params,
            source,
            diffusion_model,
            vae,
            text_encoder,
            text_encoder_vision,
            &out_path,
            init_img_path.as_deref(),
            mask_path.as_deref(),
            ref_img_path.as_deref(),
            full_checkpoint,
        );
        let mut cmd = Command::new(&sd_cli);
        cmd.args(&args);
        apply_library_path(&mut cmd, &sd_cli);

        debug!(
            target: TRACE_TARGET,
            op = "spawn",
            sd_cli = %sd_cli.display(),
            model,
            i2i = init_img_path.is_some(),
            arg_count = args.len(),
            "running sd-cli"
        );

        let started = Instant::now();
        let output = cmd
            .output()
            .with_context(|| format!("running {}", sd_cli.display()))?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                target: TRACE_TARGET,
                op = "spawn",
                model,
                elapsed_ms,
                exit = ?output.status.code(),
                stderr = %stderr,
                "sd-cli failed"
            );
            bail!(
                "sd-cli exited with {:?}: {}",
                output.status.code(),
                stderr.lines().last().unwrap_or("(no stderr)")
            );
        }

        let bytes = std::fs::read(&out_path)
            .with_context(|| format!("reading sd-cli output at {}", out_path.display()))?;
        info!(
            target: TRACE_TARGET,
            op = "dispatch",
            model,
            elapsed_ms,
            bytes = bytes.len(),
            "ok"
        );

        Ok(TaskResult::Image {
            bytes,
            ext: "webp".to_string(),
        })
    }
}

impl Engine for SdCppEngine {
    fn name(&self) -> &'static str {
        "sdcpp"
    }

    fn capabilities(&self) -> EngineCapabilities {
        // Image kind only.  The studio's selection is kind-based now
        // and the offer carries the model-source, so we don't need to
        // enumerate model names ourselves.  We still list a single
        // sentinel string so downstream code that reads
        // `supportedModels` for display sees "any sd-cpp model".
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Image, vec!["sd-cpp:*".to_string()]);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, _model: &str, _task: Task) -> Result<TaskResult> {
        bail!(
            "sdcpp engine requires a ModelSource on the offer; legacy push-based offers \
             (no modelSource) cannot be served - re-promote the job through the studio"
        )
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        match task {
            Task::Image(p) => self.dispatch_image(model, p, source),
            other => {
                // Surface the rejection at this engine's own target,
                // matching the onnx/llama/whisper/candle engines.
                // Without it an operator filtering
                // `RUST_LOG=studio_worker::engine::sdcpp=debug` sees
                // nothing when sdcpp refuses a non-image task.
                let kind = other.kind();
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    model,
                    kind = kind.as_str(),
                    "sdcpp engine only serves image jobs"
                );
                Err(crate::engine::UnsupportedTask::new("sdcpp", kind).into())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// The per-job scratch cleanup primitives (`remove_temp_file` +
// `TempFileGuard`) live in `engine::download` so this engine and the
// onnx engine share one tested implementation.

fn file_for_role(files: &[(ModelFileRole, PathBuf)], role: ModelFileRole) -> Option<&Path> {
    files
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, p)| p.as_path())
}

/// Resolve final per-job width / height / steps / cfg / sampler /
/// negative-prompt by layering `params` over `source.cli_defaults`
/// with the agreed precedence (per-job override beats model default
/// beats engine fallback).  Pure for testability.
fn resolve_image_args(params: &ImageParams, source: &ModelSource) -> ResolvedImageArgs {
    let width = if params.width > 0 {
        params.width
    } else if source.cli_defaults.width > 0 {
        source.cli_defaults.width
    } else {
        1024
    };
    let height = if params.height > 0 {
        params.height
    } else if source.cli_defaults.height > 0 {
        source.cli_defaults.height
    } else {
        1024
    };
    // Steps: per-job override wins (treat the deserialiser default of
    // 20 as "caller didn't pick" so the model's tuned step count
    // doesn't get clobbered by a stale default).
    let steps = if params.steps > 0 && params.steps != 20 {
        params.steps
    } else if source.cli_defaults.steps > 0 {
        source.cli_defaults.steps
    } else {
        STEPS_FALLBACK
    };
    let source_cfg = if source.cli_defaults.cfg_scale > 0.0 {
        source.cli_defaults.cfg_scale
    } else {
        1.0
    };
    let cfg_scale = params.cfg_scale.filter(|v| *v > 0.0).unwrap_or(source_cfg);
    let sampling_method = params
        .sampling_method
        .clone()
        .or_else(|| source.cli_defaults.sampling_method.clone());
    ResolvedImageArgs {
        width,
        height,
        steps,
        cfg_scale,
        sampling_method,
    }
}

/// Resolved per-job sd-cli numerics.  Output of [`resolve_image_args`].
#[derive(Debug, Clone, PartialEq)]
struct ResolvedImageArgs {
    width: u32,
    height: u32,
    steps: u32,
    cfg_scale: f32,
    sampling_method: Option<String>,
}

/// Build the full `sd-cli` argv for one image job.  Pure (no I/O):
/// the caller resolves files / out-path / init-image-path, this
/// function only assembles the flag list so it can be asserted in
/// unit tests without spawning the binary.
// Eight model-path + i2i components; grouping them adds indirection without
// improving readability (mirrors the `#[allow]` already used in ws::session).
#[allow(clippy::too_many_arguments)]
fn build_sdcli_args(
    params: &ImageParams,
    source: &ModelSource,
    diffusion_model: &Path,
    vae: Option<&Path>,
    text_encoder: Option<&Path>,
    text_encoder_vision: Option<&Path>,
    out_path: &Path,
    init_img_path: Option<&Path>,
    mask_path: Option<&Path>,
    ref_img_path: Option<&Path>,
    full_checkpoint: bool,
) -> Vec<OsString> {
    let resolved = resolve_image_args(params, source);
    let mut args: Vec<OsString> = Vec::with_capacity(32);

    // A full checkpoint loads via `-m`/`--model`; standalone diffusion weights via
    // `--diffusion-model` (alongside split vae/clip files).
    args.push(
        if full_checkpoint {
            "--model"
        } else {
            "--diffusion-model"
        }
        .into(),
    );
    args.push(diffusion_model.into());
    if let Some(p) = vae {
        args.push("--vae".into());
        args.push(p.into());
    }
    if let Some(p) = text_encoder {
        args.push("--llm".into());
        args.push(p.into());
    }
    if let Some(p) = text_encoder_vision {
        args.push("--llm_vision".into());
        args.push(p.into());
    }
    args.push("-p".into());
    args.push((&params.prompt as &str).into());
    if let Some(neg) = params.negative_prompt.as_deref() {
        if !neg.is_empty() {
            args.push("--negative-prompt".into());
            args.push(neg.into());
        }
    }
    if let Some(reference) = ref_img_path {
        // Reference / instruction-edit mode (Qwen-Image-Edit, Flux Kontext): the model regenerates
        // the image from the reference per the prompt. Mutually exclusive with the `--init-img`
        // img2img path. A `--mask` is honoured here too: it constrains the edit to the masked
        // region (white = editable) and leaves the rest, so the studio can place the edit inside
        // the author's drawn shape. No `--strength` (that's an img2img-only knob).
        args.push("-r".into());
        args.push(reference.into());
        if let Some(mask) = mask_path {
            args.push("--mask".into());
            args.push(mask.into());
        }
    } else if let Some(init) = init_img_path {
        args.push("--init-img".into());
        args.push(init.into());
        // `--strength` only makes sense alongside an init image
        // (sd-cli ignores it otherwise).  Default to 0.75 (sd-cli's
        // own default) when the caller didn't pick a value.
        let strength = params.denoise.unwrap_or(0.75);
        args.push("--strength".into());
        args.push(strength.to_string().into());
        // Mask-guided inpaint: only valid with an init image.
        if let Some(mask) = mask_path {
            args.push("--mask".into());
            args.push(mask.into());
        }
    }
    args.push("--cfg-scale".into());
    args.push(resolved.cfg_scale.to_string().into());
    args.push("--steps".into());
    args.push(resolved.steps.to_string().into());
    args.push("-W".into());
    args.push(resolved.width.to_string().into());
    args.push("-H".into());
    args.push(resolved.height.to_string().into());
    args.push("-o".into());
    args.push(out_path.into());
    if let Some(seed) = params.seed {
        args.push("--seed".into());
        args.push(seed.to_string().into());
    }
    if let Some(method) = resolved.sampling_method.as_deref() {
        args.push("--sampling-method".into());
        args.push(method.into());
    }
    // Flow / instruction-edit model flags (model-level constants from the registry). Only emitted
    // when the model declares them, so SDXL-style models are unaffected.
    if let Some(shift) = source.cli_defaults.flow_shift {
        args.push("--flow-shift".into());
        args.push(shift.to_string().into());
    }
    if source.cli_defaults.zero_cond_t == Some(true) {
        args.push("--qwen-image-zero-cond-t".into());
    }
    if source.cli_defaults.offload_to_cpu == Some(true) {
        args.push("--offload-to-cpu".into());
    }
    // VRAM-saving flags that are safe on every box.
    args.push("--diffusion-fa".into());
    args
}

/// Point the per-job `Command`'s dynamic linker at the shared library
/// that ships next to an auto-provisioned `sd-cli` (Linux / macOS).
/// No-op on Windows (sibling DLLs resolve automatically) and when the
/// resolved binary has no sibling library (operator wrapper-script
/// installs manage their own load path).  Prepends to any inherited
/// value so a pre-set `LD_LIBRARY_PATH` isn't clobbered.
#[cfg_attr(coverage_nightly, coverage(off))]
fn apply_library_path(cmd: &mut Command, sd_cli: &Path) {
    let Some((var, dir)) = sd_provision::library_path_env(sd_cli) else {
        return;
    };
    let value = match std::env::var_os(var) {
        Some(existing) => {
            let mut paths = vec![dir.clone()];
            paths.extend(std::env::split_paths(&existing));
            // `join_paths` only fails if a path contains the platform
            // separator; fall back to our dir alone, the entry that
            // matters for finding the sibling library.
            std::env::join_paths(paths).unwrap_or_else(|_| dir.into_os_string())
        }
        None => dir.into_os_string(),
    };
    cmd.env(var, value);
}

/// Look up `sd-cli` in env override -> `<models_root>/bin` ->
/// `~/.local/bin` -> `$PATH`.  The `<models_root>/bin` slot is where a
/// self-provisioned binary lands, so the auto-provisioner can drop it
/// next to the cached models and have the worker pick it up with no
/// PATH fiddling.  Excluded from coverage: touches several host paths
/// only one of which matches per host, and CI doesn't ship `sd-cli`.
#[cfg_attr(coverage_nightly, coverage(off))]
fn resolve_sd_cli(models_root: &Path) -> Option<PathBuf> {
    let bin = sd_provision::binary_name();
    if let Ok(p) = std::env::var("STUDIO_WORKER_SD_CLI") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let in_models = models_root.join("bin").join(bin);
    if in_models.is_file() {
        return Some(in_models);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin").join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which(bin)
}

/// `$PATH` lookup for a bare binary name.  Excluded from coverage
/// for the same reason as `resolve_sd_cli`.
#[cfg_attr(coverage_nightly, coverage(off))]
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Pick an extension to use for the init-image tempfile that sd-cli's
/// image loader can sniff.  Reads the trailing `.<ext>` from the URL's
/// path (ignoring query + fragment).  Defaults to `webp` when no
/// recognisable extension is present.
fn init_image_extension(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let lower_tail = path
        .rsplit('.')
        .next()
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_default();
    match lower_tail.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "webp" => "webp",
        "bmp" => "bmp",
        "gif" => "gif",
        "tif" | "tiff" => "tif",
        _ => "webp",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCliDefaults, ModelEngine, ModelFile, ModelFileRole};
    use tempfile::tempdir;

    fn fake_source(files: Vec<ModelFile>) -> ModelSource {
        ModelSource {
            engine: ModelEngine::SdCpp,
            files,
            cli_defaults: ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 8,
                width: 1024,
                height: 1024,
                sampling_method: Some("euler".to_string()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn file_for_role_picks_matching_file() {
        let files = vec![
            (ModelFileRole::DiffusionModel, PathBuf::from("/d.gguf")),
            (ModelFileRole::Vae, PathBuf::from("/v.safetensors")),
        ];
        assert_eq!(
            file_for_role(&files, ModelFileRole::DiffusionModel),
            Some(Path::new("/d.gguf"))
        );
        assert_eq!(
            file_for_role(&files, ModelFileRole::Vae),
            Some(Path::new("/v.safetensors"))
        );
        assert!(file_for_role(&files, ModelFileRole::TextEncoder).is_none());
    }

    #[test]
    fn ensure_files_skips_already_present() {
        let dir = tempdir().unwrap();
        let cached = dir.path().join("cached.gguf");
        std::fs::write(&cached, b"already here").unwrap();
        let engine = SdCppEngine::with_paths(PathBuf::from("/usr/bin/true"), dir.path().into());
        let source = fake_source(vec![ModelFile {
            role: ModelFileRole::DiffusionModel,
            url: "https://example.invalid/cached.gguf".into(),
            filename: "cached.gguf".into(),
            approx_bytes: None,
            sha256: None,
        }]);
        let resolved = engine.ensure_files(&source).expect("cached file used");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, ModelFileRole::DiffusionModel);
        assert_eq!(resolved[0].1, cached);
        // Untouched on disk — our "download" never ran.
        assert_eq!(std::fs::read(&cached).unwrap(), b"already here");
    }

    #[test]
    fn dispatch_rejects_non_image_tasks() {
        use crate::types::AudioTtsParams;
        let dir = tempdir().unwrap();
        let engine = SdCppEngine::with_paths(PathBuf::from("/usr/bin/true"), dir.path().into());
        let task = Task::AudioTts(AudioTtsParams {
            text: "hi".into(),
            voice: "v".into(),
            ext: "wav".into(),
            ..Default::default()
        });
        let source = fake_source(vec![]);
        let err = engine
            .dispatch_with_source("anything", task, &source)
            .unwrap_err();
        assert!(err.to_string().contains("cannot serve audio_tts"));
    }

    // The legacy `dispatch_requires_model_source` test is gone: the
    // trait signature now takes `&ModelSource` so the compiler enforces
    // it at every call site.  No runtime fallback to police.

    // -----------------------------------------------------------------
    // Pure arg-builder tests — lock down the sd-cli invocation contract
    // without needing the binary on the box.
    // -----------------------------------------------------------------

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    fn idx_after(args: &[String], flag: &str) -> Option<usize> {
        args.iter().position(|a| a == flag).map(|i| i + 1)
    }

    #[test]
    fn build_sdcli_args_includes_required_flags() {
        let params = ImageParams {
            prompt: "hello".into(),
            width: 768,
            height: 512,
            steps: 20, // "caller didn't pick" → source default wins
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            Some(Path::new("/v.safetensors")),
            Some(Path::new("/llm.gguf")),
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--diffusion-model").unwrap()], "/d.gguf");
        assert_eq!(s[idx_after(&s, "--vae").unwrap()], "/v.safetensors");
        assert_eq!(s[idx_after(&s, "--llm").unwrap()], "/llm.gguf");
        assert_eq!(s[idx_after(&s, "-p").unwrap()], "hello");
        assert_eq!(s[idx_after(&s, "-W").unwrap()], "768");
        assert_eq!(s[idx_after(&s, "-H").unwrap()], "512");
        // source default cfg_scale=1.0
        assert_eq!(s[idx_after(&s, "--cfg-scale").unwrap()], "1");
        // source default steps=8 wins (param.steps==20 treated as default)
        assert_eq!(s[idx_after(&s, "--steps").unwrap()], "8");
        assert_eq!(s[idx_after(&s, "--sampling-method").unwrap()], "euler");
        assert_eq!(s[idx_after(&s, "-o").unwrap()], "/tmp/out.webp");
        assert!(s.contains(&"--diffusion-fa".to_string()));
        // Never includes init-only flags when no init image present.
        assert!(!s.contains(&"--init-img".to_string()));
        assert!(!s.contains(&"--strength".to_string()));
    }

    #[test]
    fn build_sdcli_args_includes_negative_prompt_when_set() {
        let params = ImageParams {
            prompt: "hi".into(),
            negative_prompt: Some("text, watermark, low quality".into()),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(
            s[idx_after(&s, "--negative-prompt").unwrap()],
            "text, watermark, low quality"
        );
    }

    #[test]
    fn build_sdcli_args_omits_negative_prompt_when_empty_string() {
        let params = ImageParams {
            prompt: "hi".into(),
            negative_prompt: Some(String::new()),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert!(!s.contains(&"--negative-prompt".to_string()));
    }

    #[test]
    fn build_sdcli_args_includes_init_image_and_strength() {
        let params = ImageParams {
            prompt: "hi".into(),
            denoise: Some(0.55),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            Some(Path::new("/tmp/init.webp")),
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--init-img").unwrap()], "/tmp/init.webp");
        assert_eq!(s[idx_after(&s, "--strength").unwrap()], "0.55");
        // No mask supplied → no inpaint flag.
        assert!(!s.contains(&"--mask".to_string()));
    }

    #[test]
    fn build_sdcli_args_includes_mask_for_inpaint() {
        let params = ImageParams {
            prompt: "remove the tree".into(),
            denoise: Some(0.8),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            Some(Path::new("/tmp/init.webp")),
            Some(Path::new("/tmp/mask.png")),
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--init-img").unwrap()], "/tmp/init.webp");
        assert_eq!(s[idx_after(&s, "--mask").unwrap()], "/tmp/mask.png");
        assert_eq!(s[idx_after(&s, "--strength").unwrap()], "0.8");
    }

    #[test]
    fn build_sdcli_args_uses_model_flag_for_full_checkpoint() {
        let params = ImageParams {
            prompt: "hi".into(),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/checkpoint.safetensors"),
            Some(Path::new("/v.safetensors")),
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            true,
        );
        let s = args_to_strings(&args);
        // A full checkpoint loads via -m/--model, not --diffusion-model.
        assert_eq!(
            s[idx_after(&s, "--model").unwrap()],
            "/checkpoint.safetensors"
        );
        assert!(!s.contains(&"--diffusion-model".to_string()));
    }

    #[test]
    fn build_sdcli_args_defaults_denoise_when_init_image_present_but_denoise_none() {
        let params = ImageParams {
            prompt: "hi".into(),
            denoise: None,
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            Some(Path::new("/tmp/init.webp")),
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--strength").unwrap()], "0.75");
    }

    #[test]
    fn build_sdcli_args_per_job_cfg_scale_overrides_model_default() {
        let params = ImageParams {
            prompt: "hi".into(),
            cfg_scale: Some(7.5),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--cfg-scale").unwrap()], "7.5");
    }

    #[test]
    fn build_sdcli_args_per_job_sampling_method_overrides_model_default() {
        let params = ImageParams {
            prompt: "hi".into(),
            sampling_method: Some("dpm++2m".into()),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--sampling-method").unwrap()], "dpm++2m");
    }

    #[test]
    fn build_sdcli_args_per_job_steps_overrides_when_non_default() {
        let params = ImageParams {
            prompt: "hi".into(),
            steps: 30, // != 20 → treat as caller override
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--steps").unwrap()], "30");
    }

    #[test]
    fn build_sdcli_args_seed_included_when_set() {
        let params = ImageParams {
            prompt: "hi".into(),
            seed: Some(42),
            ..Default::default()
        };
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--seed").unwrap()], "42");
    }

    /// A model source carrying the Qwen-Image-Edit flow flags.
    fn qwen_edit_source() -> ModelSource {
        ModelSource {
            engine: ModelEngine::SdCpp,
            files: vec![],
            cli_defaults: ModelCliDefaults {
                cfg_scale: 4.0,
                steps: 20,
                width: 1024,
                height: 1024,
                sampling_method: Some("euler".to_string()),
                flow_shift: Some(3.0),
                zero_cond_t: Some(true),
                offload_to_cpu: Some(true),
            },
        }
    }

    #[test]
    fn build_sdcli_args_reference_mode_for_instruction_edit() {
        let params = ImageParams {
            prompt: "add a red beach ball".into(),
            denoise: Some(0.9),
            ..Default::default()
        };
        let source = qwen_edit_source();
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/qwen.gguf"),
            Some(Path::new("/vae.safetensors")),
            Some(Path::new("/llm.gguf")),
            Some(Path::new("/mmproj.gguf")),
            Path::new("/tmp/out.webp"),
            None,
            Some(Path::new("/tmp/mask.png")),
            Some(Path::new("/tmp/ref.webp")),
            false,
        );
        let s = args_to_strings(&args);
        // Reference mode: `-r` set, a `--mask` constrains the edit region, and the img2img-only
        // `--init-img` / `--strength` flags are suppressed.
        assert_eq!(s[idx_after(&s, "-r").unwrap()], "/tmp/ref.webp");
        assert_eq!(s[idx_after(&s, "--mask").unwrap()], "/tmp/mask.png");
        assert!(!s.contains(&"--init-img".to_string()));
        assert!(!s.contains(&"--strength".to_string()));
        // Vision encoder + Qwen flow flags emitted.
        assert_eq!(s[idx_after(&s, "--llm_vision").unwrap()], "/mmproj.gguf");
        assert_eq!(s[idx_after(&s, "--flow-shift").unwrap()], "3");
        assert!(s.contains(&"--qwen-image-zero-cond-t".to_string()));
        assert!(s.contains(&"--offload-to-cpu".to_string()));
    }

    #[test]
    fn build_sdcli_args_omits_qwen_flags_for_plain_model() {
        let params = ImageParams {
            prompt: "hi".into(),
            ..Default::default()
        };
        // fake_source has no flow_shift / zero_cond_t / offload_to_cpu.
        let source = fake_source(vec![]);
        let args = build_sdcli_args(
            &params,
            &source,
            Path::new("/d.gguf"),
            None,
            None,
            None,
            Path::new("/tmp/out.webp"),
            None,
            None,
            None,
            false,
        );
        let s = args_to_strings(&args);
        assert!(!s.contains(&"--flow-shift".to_string()));
        assert!(!s.contains(&"--qwen-image-zero-cond-t".to_string()));
        assert!(!s.contains(&"--offload-to-cpu".to_string()));
        assert!(!s.contains(&"--llm_vision".to_string()));
        assert!(!s.contains(&"-r".to_string()));
    }

    #[test]
    fn capabilities_advertises_only_image_kind() {
        let dir = tempdir().unwrap();
        let engine = SdCppEngine::with_paths(PathBuf::from("/usr/bin/true"), dir.path().into());
        let caps = engine.capabilities();
        assert!(caps
            .supported_models_per_kind
            .contains_key(&TaskKind::Image));
        assert_eq!(caps.supported_models_per_kind.len(), 1);
    }

    #[test]
    fn init_image_extension_reads_url_tail() {
        assert_eq!(init_image_extension("https://x/y/latest.webp"), "webp");
        assert_eq!(init_image_extension("https://x/y/latest.PNG"), "png");
        assert_eq!(init_image_extension("https://x/y/latest.jpg"), "jpg");
        assert_eq!(init_image_extension("https://x/y/latest.jpeg"), "jpg");
        // Query strings + fragments don't trick the parser.
        assert_eq!(
            init_image_extension("https://x/y/latest.webp?v=42&t=now"),
            "webp"
        );
        assert_eq!(init_image_extension("https://x/y/latest.webp#frag"), "webp");
        // Unknown extension falls back to webp.
        assert_eq!(
            init_image_extension("https://x/y/latest.unknownext"),
            "webp"
        );
        assert_eq!(init_image_extension("https://x/y/no-ext"), "webp");
    }
}
