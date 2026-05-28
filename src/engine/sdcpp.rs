//! Engine that runs real image inference by subprocess-invoking the
//! `stable-diffusion.cpp` (`sd-cli`) binary.
//!
//! The studio's offer carries a [`ModelSource`] with everything we
//! need: an engine identifier (`sd-cpp`), the list of files to
//! download (diffusion-model + text-encoder + VAE, each with a public
//! URL + filename), and CLI defaults (cfg-scale, steps, dimensions).
//! The worker has zero hardcoded model knowledge \u2014 it caches
//! whatever the studio asks for under `cfg.models_root` and invokes
//! `sd-cli` with the files arranged by role.
//!
//! Layout under `cfg.models_root` (default `~/models`):
//! ```text
//! ~/models/<filename1>
//! ~/models/<filename2>
//! \u2026
//! ```
//! Files are downloaded on first use (HEAD-checked length so we don't
//! re-download something that's already there) and re-used across
//! every subsequent job that names them.
//!
//! The engine self-registers only when `sd-cli` is present on the box
//! (either at `$STUDIO_WORKER_SD_CLI`, or `~/.local/bin/sd-cli`, or on
//! `$PATH`).  Without `sd-cli` the worker can't run real-image jobs
//! at all so it skips registration and the multi engine falls through
//! to synthetic for any kind it doesn't have a real backend for.

use crate::engine::{Engine, EngineCapabilities};
use crate::types::{ImageParams, ModelFileRole, ModelSource, Task, TaskKind, TaskResult};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
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

/// HTTP client timeout per request \u2014 the GGUF download is up to a few
/// GiB so a 30-minute ceiling is generous.
const DOWNLOAD_TIMEOUT_SECS: u64 = 30 * 60;

/// Worker-side engine that drives `sd-cli` per job.
pub struct SdCppEngine {
    sd_cli: PathBuf,
    models_root: PathBuf,
}

impl SdCppEngine {
    /// Try to build the engine; returns `None` if `sd-cli` isn't on
    /// the box.  The model files come in on the offer so we don't
    /// need to pre-stage anything.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn try_new(models_root: &Path) -> Option<Self> {
        let sd_cli = resolve_sd_cli()?;
        if let Err(e) = std::fs::create_dir_all(models_root) {
            warn!(
                target: TRACE_TARGET,
                models_root = %models_root.display(),
                error = %e,
                "could not create models_root; skipping sdcpp registration"
            );
            return None;
        }
        info!(
            target: TRACE_TARGET,
            sd_cli = %sd_cli.display(),
            models_root = %models_root.display(),
            "sdcpp engine registered"
        );
        Some(Self {
            sd_cli,
            models_root: models_root.to_path_buf(),
        })
    }

    /// For tests: build with explicit paths (bypasses sd-cli lookup).
    #[cfg(test)]
    pub fn with_paths(sd_cli: PathBuf, models_root: PathBuf) -> Self {
        Self {
            sd_cli,
            models_root,
        }
    }

    /// Ensure each file in `source.files` is present under
    /// `self.models_root`.  Downloads anything missing.  Returns the
    /// resolved local path for each file (in the same order).
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_files(&self, source: &ModelSource) -> Result<Vec<(ModelFileRole, PathBuf)>> {
        let mut out = Vec::with_capacity(source.files.len());
        for file in &source.files {
            let local = self.models_root.join(&file.filename);
            if !local.is_file() {
                download_file(&file.url, &local).with_context(|| {
                    format!(
                        "downloading {} ({}) -> {}",
                        file.filename,
                        file.url,
                        local.display()
                    )
                })?;
            } else {
                debug!(
                    target: TRACE_TARGET,
                    op = "ensure_file",
                    filename = %file.filename,
                    path = %local.display(),
                    "cached"
                );
            }
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
        let files = self.ensure_files(source)?;
        let diffusion_model = file_for_role(&files, ModelFileRole::DiffusionModel)
            .or_else(|| file_for_role(&files, ModelFileRole::Model))
            .ok_or_else(|| anyhow!("modelSource has no diffusion-model / model file"))?;
        let vae = file_for_role(&files, ModelFileRole::Vae);
        let text_encoder = file_for_role(&files, ModelFileRole::TextEncoder);

        let out_dir = std::env::temp_dir().join("studio-worker-sdcpp");
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating sdcpp output dir {}", out_dir.display()))?;
        let stem = format!(
            "out-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let out_path = out_dir.join(format!("{stem}.webp"));

        // Resolution + per-call params come from the OFFER's task
        // payload first (game manifests pin per-asset dimensions, e.g.
        // game-of-elements = 768x512).  The model's cliDefaults are
        // a fallback for offers that don't carry task params.
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
        // Steps: same priority — task first, model second, fallback last.
        // Note: ImageParams::default() is 20 steps; we treat that as
        // "caller didn't pick" so the model's tuned step count wins.
        let steps = if params.steps > 0 && params.steps != 20 {
            params.steps
        } else if source.cli_defaults.steps > 0 {
            source.cli_defaults.steps
        } else {
            STEPS_FALLBACK
        };
        let cfg_scale = if source.cli_defaults.cfg_scale > 0.0 {
            source.cli_defaults.cfg_scale
        } else {
            1.0
        };

        let mut cmd = Command::new(&self.sd_cli);
        cmd.arg("--diffusion-model").arg(diffusion_model);
        if let Some(p) = vae {
            cmd.arg("--vae").arg(p);
        }
        if let Some(p) = text_encoder {
            cmd.arg("--llm").arg(p);
        }
        cmd.arg("-p")
            .arg(&params.prompt)
            .arg("--cfg-scale")
            .arg(cfg_scale.to_string())
            .arg("--steps")
            .arg(steps.to_string())
            .arg("-W")
            .arg(width.to_string())
            .arg("-H")
            .arg(height.to_string())
            .arg("-o")
            .arg(&out_path);
        if let Some(seed) = params.seed {
            cmd.arg("--seed").arg(seed.to_string());
        }
        if let Some(ref method) = source.cli_defaults.sampling_method {
            cmd.arg("--sampling-method").arg(method);
        }
        // VRAM-saving flags that are safe on every box.
        cmd.arg("--diffusion-fa");

        debug!(
            target: TRACE_TARGET,
            op = "spawn",
            sd_cli = %self.sd_cli.display(),
            model,
            steps,
            width,
            height,
            "running sd-cli"
        );

        let started = Instant::now();
        let output = cmd
            .output()
            .with_context(|| format!("running {}", self.sd_cli.display()))?;
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
        let _ = std::fs::remove_file(&out_path);
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
        source: Option<&ModelSource>,
    ) -> Result<TaskResult> {
        let kind = task.kind();
        let source =
            source.ok_or_else(|| anyhow!("sdcpp engine requires a ModelSource on the offer"))?;
        match task {
            Task::Image(p) => self.dispatch_image(model, p, source),
            _ => bail!("sdcpp engine cannot serve {} tasks", kind.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_for_role(files: &[(ModelFileRole, PathBuf)], role: ModelFileRole) -> Option<&Path> {
    files
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, p)| p.as_path())
}

/// Stream `url` into `dest` (atomic via a `.part` rename so a killed
/// download doesn't leave a half-written file on disk).
///
/// Excluded from coverage: requires real network + filesystem (and
/// a 5GB download per model on the happy path).  Exercised
/// end-to-end via the live dev loop.
#[cfg_attr(coverage_nightly, coverage(off))]
fn download_file(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = dest.with_extension("part");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()?;
    info!(
        target: TRACE_TARGET,
        op = "download",
        url,
        dest = %dest.display(),
        "starting"
    );
    let started = Instant::now();
    let mut response = client.get(url).send().context("GET")?;
    if !response.status().is_success() {
        bail!("GET {url} -> {}", response.status());
    }
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let bytes = std::io::copy(&mut response, &mut file).context("streaming body")?;
    drop(file);
    std::fs::rename(&part, dest)
        .with_context(|| format!("renaming {} -> {}", part.display(), dest.display()))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    info!(
        target: TRACE_TARGET,
        op = "download",
        url,
        dest = %dest.display(),
        bytes,
        elapsed_ms,
        "done"
    );
    Ok(())
}

/// Look up `sd-cli` in env override -> `~/.local/bin` -> `$PATH`.
/// Look for the `sd-cli` binary on the box.  Excluded from coverage:
/// touches `$STUDIO_WORKER_SD_CLI`, `~/.local/bin/sd-cli`, and `$PATH`
/// in order — only one of which matches at a time per host, and CI
/// doesn't ship `sd-cli` at all.
#[cfg_attr(coverage_nightly, coverage(off))]
fn resolve_sd_cli() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STUDIO_WORKER_SD_CLI") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/sd-cli");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which("sd-cli")
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
        }]);
        let resolved = engine.ensure_files(&source).expect("cached file used");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, ModelFileRole::DiffusionModel);
        assert_eq!(resolved[0].1, cached);
        // Untouched on disk \u2014 our "download" never ran.
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
        });
        let source = fake_source(vec![]);
        let err = engine
            .dispatch_with_source("anything", task, Some(&source))
            .unwrap_err();
        assert!(err.to_string().contains("cannot serve audio_tts"));
    }

    #[test]
    fn dispatch_requires_model_source() {
        use crate::types::ImageParams;
        let dir = tempdir().unwrap();
        let engine = SdCppEngine::with_paths(PathBuf::from("/usr/bin/true"), dir.path().into());
        let task = Task::Image(ImageParams {
            prompt: "x".into(),
            width: 64,
            height: 64,
            steps: 1,
            seed: None,
            ext: "webp".into(),
        });
        let err = engine
            .dispatch_with_source("z-image-turbo-q4_k_m.gguf", task, None)
            .unwrap_err();
        assert!(err.to_string().contains("requires"));
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
}
