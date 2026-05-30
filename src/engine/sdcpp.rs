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

use crate::engine::{Engine, EngineCapabilities};
use crate::types::{ImageParams, ModelFileRole, ModelSource, Task, TaskKind, TaskResult};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
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
            let local = model_cache_path(&self.models_root, &file.filename)?;
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

        // Own the scratch files from the moment their paths exist so
        // every failure path (sd-cli error, unreadable output) cleans
        // up instead of leaking them into the temp dir.
        let mut temp_files = TempFileGuard::new();
        temp_files.push(out_path.clone());

        // If the task carries an init image URL, stream it to a
        // tempfile so we can hand the path to `sd-cli --init-img`.
        // This is mandatory — the worker refuses i2i jobs whose
        // init image fails to download (no silent fallback to t2i).
        // The local extension mirrors the URL's so sd-cli's image
        // loader can sniff the format.
        let init_img_path = match params.init_image_url.as_deref() {
            Some(url) if !url.is_empty() => {
                let ext = init_image_extension(url);
                let init_path = out_dir.join(format!("{stem}-init.{ext}"));
                download_file(url, &init_path).with_context(|| {
                    format!("downloading init image {} -> {}", url, init_path.display())
                })?;
                temp_files.push(init_path.clone());
                Some(init_path)
            }
            _ => None,
        };

        let args = build_sdcli_args(
            &params,
            source,
            diffusion_model,
            vae,
            text_encoder,
            &out_path,
            init_img_path.as_deref(),
        );
        let mut cmd = Command::new(&self.sd_cli);
        cmd.args(&args);

        debug!(
            target: TRACE_TARGET,
            op = "spawn",
            sd_cli = %self.sd_cli.display(),
            model,
            i2i = init_img_path.is_some(),
            arg_count = args.len(),
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
        let kind = task.kind();
        match task {
            Task::Image(p) => self.dispatch_image(model, p, source),
            _ => bail!("sdcpp engine cannot serve {} tasks", kind.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort removal of a temporary file (a per-job `sd-cli` output,
/// an init image, or a half-written `.part` download).  Removal is
/// non-fatal — the artefact has already been read or the job already
/// failed — but a remove that keeps failing silently leaks temp files
/// and can quietly fill the worker's disk over a long-running session,
/// so we surface the failure instead of swallowing it.  A `NotFound`
/// is the desired end state (something already cleaned it up), so it's
/// not logged.
fn remove_temp_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                target: TRACE_TARGET,
                op = "cleanup",
                path = %path.display(),
                error = %e,
                "failed to remove temp file"
            );
        }
    }
}

/// RAII owner of a job's scratch files (the `sd-cli` output image and a
/// downloaded init image).  Registering them up front means every exit
/// path - the success return, an `sd-cli` non-zero exit, an unreadable
/// output file, even a panic - removes them on drop instead of leaking
/// them into the temp dir and slowly filling the worker's disk over a
/// long-running session.  Removal is best-effort via [`remove_temp_file`],
/// so a path that never materialised (job failed before `sd-cli` wrote
/// anything) is silently tolerated.
struct TempFileGuard {
    paths: Vec<PathBuf>,
}

impl TempFileGuard {
    fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn push(&mut self, path: PathBuf) {
        self.paths.push(path);
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            remove_temp_file(path);
        }
    }
}

/// Verify a streamed download wrote exactly the body the server
/// promised.  `expected` is the response's `Content-Length`; it's
/// `None` for chunked transfers, where there's nothing to check and
/// we accept whatever arrived (the behaviour before this guard).  A
/// mismatch in either direction means the download is truncated or
/// corrupt, so we surface a clear error rather than cache a bad model.
fn verify_download_len(copied: u64, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if copied != expected => bail!(
            "size mismatch: wrote {copied} bytes but the server declared \
             Content-Length {expected} (download truncated or corrupt)"
        ),
        _ => Ok(()),
    }
}

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
    // The server's Content-Length (absent on chunked transfers) is what
    // lets us catch a short read before committing the file.
    let expected_len = response.content_length();
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let copied = std::io::copy(&mut response, &mut file);
    // Close the handle before any remove / rename so the cleanup path
    // works on Windows, where an open file can't be unlinked.
    drop(file);
    let bytes = match copied {
        Ok(bytes) => bytes,
        Err(e) => {
            remove_temp_file(&part);
            return Err(e).context("streaming body");
        }
    };
    if let Err(e) = verify_download_len(bytes, expected_len) {
        remove_temp_file(&part);
        return Err(e).with_context(|| format!("downloading {url}"));
    }
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
fn build_sdcli_args(
    params: &ImageParams,
    source: &ModelSource,
    diffusion_model: &Path,
    vae: Option<&Path>,
    text_encoder: Option<&Path>,
    out_path: &Path,
    init_img_path: Option<&Path>,
) -> Vec<OsString> {
    let resolved = resolve_image_args(params, source);
    let mut args: Vec<OsString> = Vec::with_capacity(32);

    args.push("--diffusion-model".into());
    args.push(diffusion_model.into());
    if let Some(p) = vae {
        args.push("--vae".into());
        args.push(p.into());
    }
    if let Some(p) = text_encoder {
        args.push("--llm".into());
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
    if let Some(init) = init_img_path {
        args.push("--init-img".into());
        args.push(init.into());
        // `--strength` only makes sense alongside an init image
        // (sd-cli ignores it otherwise).  Default to 0.75 (sd-cli's
        // own default) when the caller didn't pick a value.
        let strength = params.denoise.unwrap_or(0.75);
        args.push("--strength".into());
        args.push(strength.to_string().into());
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
    // VRAM-saving flags that are safe on every box.
    args.push("--diffusion-fa".into());
    args
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

fn model_cache_path(models_root: &Path, filename: &str) -> Result<PathBuf> {
    let path = Path::new(filename);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None)
            if !filename.contains('/') && !filename.contains('\\') =>
        {
            Ok(models_root.join(name))
        }
        _ => bail!("model filename must be a plain file name: {filename:?}"),
    }
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
            },
        }
    }

    #[test]
    fn temp_file_guard_removes_every_registered_file_on_drop() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.webp");
        let init = dir.path().join("out-init.png");
        std::fs::write(&out, b"image").unwrap();
        std::fs::write(&init, b"init").unwrap();
        {
            let mut guard = TempFileGuard::new();
            guard.push(out.clone());
            guard.push(init.clone());
            assert!(out.exists() && init.exists(), "files present before drop");
        }
        assert!(!out.exists(), "sd-cli output temp must be removed on drop");
        assert!(!init.exists(), "init-image temp must be removed on drop");
    }

    #[test]
    fn temp_file_guard_tolerates_a_file_that_never_materialised() {
        // The output path is registered before sd-cli runs, so a job
        // that fails before writing anything drops a guard pointing at
        // a path that never existed.  That is the desired end state,
        // not a cleanup warning.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-written.webp");
        let out = crate::test_support::capture(move || {
            let mut guard = TempFileGuard::new();
            guard.push(missing);
            drop(guard);
        });
        assert!(
            !out.contains("failed to remove temp file"),
            "a never-created temp file must not warn on cleanup: {out:?}"
        );
    }

    #[test]
    fn remove_temp_file_deletes_an_existing_file_quietly() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("artefact.webp");
        std::fs::write(&f, b"bytes").unwrap();
        let out = crate::test_support::capture({
            let f = f.clone();
            move || remove_temp_file(&f)
        });
        assert!(!f.exists(), "file should be gone after cleanup");
        assert!(
            !out.contains("failed to remove temp file"),
            "the success path must not warn: {out:?}"
        );
    }

    #[test]
    fn remove_temp_file_ignores_an_already_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-existed.webp");
        let out = crate::test_support::capture(move || remove_temp_file(&missing));
        assert!(
            !out.contains("failed to remove temp file"),
            "a not-found file is the desired end state, not a warning: {out:?}"
        );
    }

    #[test]
    fn remove_temp_file_surfaces_a_failed_removal() {
        // Pointing the helper at a directory makes `remove_file` fail
        // on every platform (it refuses to unlink a dir): the closest
        // portable stand-in for a locked / permission-denied temp file.
        let dir = tempdir().unwrap();
        let stubborn = dir.path().join("subdir");
        std::fs::create_dir(&stubborn).unwrap();
        let out = crate::test_support::capture(move || remove_temp_file(&stubborn));
        assert!(
            out.contains("failed to remove temp file"),
            "a failed removal must surface in the logs: {out:?}"
        );
        assert!(
            out.contains("subdir"),
            "the warning must name the offending path: {out:?}"
        );
        assert!(
            out.contains("cleanup"),
            "the warning should tag the cleanup op: {out:?}"
        );
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
    fn model_cache_path_accepts_plain_filenames_only() {
        let root = Path::new("/models");
        assert_eq!(
            model_cache_path(root, "model.gguf").unwrap(),
            PathBuf::from("/models/model.gguf")
        );
        assert!(model_cache_path(root, "../outside.gguf").is_err());
        assert!(model_cache_path(root, "nested/model.gguf").is_err());
        assert!(model_cache_path(root, "/tmp/model.gguf").is_err());
        assert!(model_cache_path(root, r"nested\model.gguf").is_err());
        assert!(model_cache_path(root, "").is_err());
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            Some(Path::new("/tmp/init.webp")),
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--init-img").unwrap()], "/tmp/init.webp");
        assert_eq!(s[idx_after(&s, "--strength").unwrap()], "0.55");
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
            Path::new("/tmp/out.webp"),
            Some(Path::new("/tmp/init.webp")),
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            None,
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
            Path::new("/tmp/out.webp"),
            None,
        );
        let s = args_to_strings(&args);
        assert_eq!(s[idx_after(&s, "--seed").unwrap()], "42");
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

    #[test]
    fn verify_download_len_accepts_exact_match() {
        assert!(verify_download_len(2_700_000_000, Some(2_700_000_000)).is_ok());
    }

    #[test]
    fn verify_download_len_accepts_when_length_unknown() {
        // Chunked transfers omit Content-Length; we can't check, so we
        // accept whatever streamed in (same as before this guard).
        assert!(verify_download_len(123, None).is_ok());
    }

    #[test]
    fn verify_download_len_rejects_truncated_download() {
        let err = verify_download_len(40, Some(100)).unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "got: {err}");
        assert!(err.contains("40"), "got: {err}");
        assert!(err.contains("100"), "got: {err}");
    }

    #[test]
    fn verify_download_len_rejects_overlong_download() {
        // A body longer than the declared length is just as corrupt as a
        // short one - reject both rather than caching a bad model.
        assert!(verify_download_len(120, Some(100)).is_err());
    }
}
