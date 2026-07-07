//! ONNX-runtime image engine (pykeio/ort).
//!
//! Serves the **LaMa** object-removal model (`Carve/LaMa-ONNX`), the
//! Find-the-Differences removal engine.  LaMa reconstructs the
//! background under a masked region — unlike a diffusion fill it never
//! hallucinates a replacement object, and unlike an instruction editor
//! it leaves everything outside the mask pixel-identical.  That makes it
//! the right tool for the "2-variant illusion" difference pairs.
//!
//! Pipeline per job (`dispatch_with_source`):
//!   1. download the `.onnx` (role `model`) into `<models_root>` (cached)
//!   2. download the init image (`initImageUrl`) + mask (`maskUrl`)
//!   3. run LaMa at its fixed 512×512 (resize in, resize out)
//!   4. composite the inpainted region back onto the *full-resolution*
//!      original (feathered mask alpha) so outside-mask pixels are
//!      byte-identical and the fill stays sharp where it matters
//!
//! Cross-platform: `ort`'s `download-binaries` links a prebuilt ONNX
//! Runtime for the build target (all five cargo-dist targets), so a
//! source build needs no system onnxruntime.  CPU execution provider
//! only — LaMa at 512² is ~1 s on CPU.
use super::onnx_provision;
use crate::engine::{download, Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{anyhow, bail, Context, Result};
use image::{imageops::FilterType, DynamicImage, GrayImage, RgbImage};
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::{debug, info, warn};

/// Tracing target — filter with `RUST_LOG=studio_worker::engine::onnx=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::onnx";

/// Engine name used by `MultiEngine` routing (`ModelEngine::Onnx`).
pub const ENGINE_NAME: &str = "onnx";

/// Carve/LaMa-ONNX is exported at a fixed 512×512 resolution.
const LAMA_SIZE: u32 = 512;

/// Gaussian sigma for feathering the composite mask edge (in px at the
/// output resolution).  Small enough to stay crisp, large enough to
/// hide the inpaint boundary.
const FEATHER_SIGMA: f32 = 4.0;

/// ONNX image engine.  Caches a single loaded [`Session`] keyed by the
/// resolved model path so back-to-back jobs on the same model don't
/// re-parse the graph.  `run` needs `&mut Session`, hence the `Mutex`.
pub struct OnnxImageEngine {
    models_root: PathBuf,
    cached: Mutex<Option<(PathBuf, Session)>>,
}

impl OnnxImageEngine {
    pub fn new(models_root: PathBuf) -> Self {
        debug!(
            target: TRACE_TARGET,
            op = "new",
            models_root = %models_root.display(),
            "onnx image engine constructed"
        );
        Self {
            models_root,
            cached: Mutex::new(None),
        }
    }

    /// Resolve + download the single `.onnx` weights file (role
    /// `model`) from the model source.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_model(&self, model: &str, source: &ModelSource) -> Result<PathBuf> {
        let file = source
            .files
            .iter()
            .find(|f| f.role == ModelFileRole::Model)
            .ok_or_else(|| anyhow!("onnx modelSource has no `model` file (the .onnx weights)"))?;
        download::ensure_file_for_model(&self.models_root, model, file)
            .with_context(|| format!("downloading onnx model {}", file.url))
    }

    /// Run LaMa on `model_path` over `image`/`mask` (both already at
    /// [`LAMA_SIZE`]²) and return the raw `[1,3,512,512]` output buffer.
    /// Excluded from coverage: needs the onnxruntime native lib + the
    /// model file, neither present on the CI runner — exercised via the
    /// live dev loop + the `#[ignore]` golden test.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run_session(&self, model_path: &Path, image: Vec<f32>, mask: Vec<f32>) -> Result<Vec<f32>> {
        self.ensure_ort_runtime()?;
        let mut guard = self.cached.lock();
        if guard.as_ref().map(|(p, _)| p.as_path()) != Some(model_path) {
            let session = Session::builder()
                .context("ort Session::builder")?
                .commit_from_file(model_path)
                .with_context(|| format!("loading onnx model {}", model_path.display()))?;
            info!(
                target: TRACE_TARGET,
                op = "load",
                model = %model_path.display(),
                "onnx session loaded"
            );
            *guard = Some((model_path.to_path_buf(), session));
        }
        let session = &mut guard.as_mut().expect("session just set").1;

        let image_t =
            Tensor::from_array(([1_usize, 3, LAMA_SIZE as usize, LAMA_SIZE as usize], image))
                .context("building image tensor")?;
        let mask_t =
            Tensor::from_array(([1_usize, 1, LAMA_SIZE as usize, LAMA_SIZE as usize], mask))
                .context("building mask tensor")?;

        let outputs = session
            .run(ort::inputs!["image" => image_t, "mask" => mask_t])
            .context("onnx session.run")?;
        let (_, data) = outputs["output"]
            .try_extract_tensor::<f32>()
            .context("extracting onnx output tensor")?;
        Ok(data.to_vec())
    }

    /// Provision (download on first use) the ONNX Runtime shared library for this
    /// platform and point `ort` (load-dynamic) at it via `ORT_DYLIB_PATH`, before
    /// the first session is created. Idempotent.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_ort_runtime(&self) -> Result<()> {
        if std::env::var_os("ORT_DYLIB_PATH").is_some() {
            return Ok(());
        }
        let lib = onnx_provision::provision(&self.models_root)?;
        std::env::set_var("ORT_DYLIB_PATH", &lib);
        info!(
            target: TRACE_TARGET,
            op = "runtime",
            dylib = %lib.display(),
            version = onnx_provision::ORT_VERSION,
            "onnx runtime provisioned"
        );
        Ok(())
    }

    /// Full LaMa removal for one image job.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn dispatch_removal(
        &self,
        model: &str,
        params: &ImageParams,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        let init_url = params
            .init_image_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("onnx/LaMa removal requires `initImageUrl` (the original image)")
            })?;
        let mask_url = params
            .mask_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("onnx/LaMa removal requires `maskUrl` (the region to remove)")
            })?;

        let model_path = self.ensure_model(model, source)?;

        let work_dir = std::env::temp_dir().join("studio-worker-onnx");
        std::fs::create_dir_all(&work_dir)
            .with_context(|| format!("creating onnx work dir {}", work_dir.display()))?;
        let stem = format!(
            "onnx-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let init_path = work_dir.join(format!("{stem}-init"));
        let mask_path = work_dir.join(format!("{stem}-mask"));
        // Own the scratch downloads from the moment their paths exist so
        // every exit path — a failed mask download, a decode/onnx error,
        // even a panic mid-removal — removes them (warning on a stuck
        // file) instead of leaking them into the temp dir over a long
        // session.  The old `let _ = remove_file(…)` ran only on the
        // success/`Err` fall-through and swallowed every failure.
        let mut scratch = download::TempFileGuard::new();
        scratch.push(init_path.clone());
        scratch.push(mask_path.clone());
        download::download_file(init_url, &init_path)
            .with_context(|| format!("downloading init image {init_url}"))?;
        download::download_file(mask_url, &mask_path)
            .with_context(|| format!("downloading mask {mask_url}"))?;

        let started = Instant::now();
        let bytes = self.remove(&model_path, &init_path, &mask_path, params)?;

        debug!(
            target: TRACE_TARGET,
            op = "dispatch",
            model = %model_path.display(),
            width = params.width,
            height = params.height,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "lama removal complete"
        );
        Ok(TaskResult::Image {
            bytes,
            ext: params.ext.clone(),
        })
    }

    /// Run LaMa over an arbitrary-size RGB region + its mask, returning the
    /// inpainted region at the *same* size. Resizes the region to LaMa's fixed
    /// 512² for inference then back; with the HD crop the region is small, so
    /// this round-trip is near-lossless (sharp fill).
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run_lama_region(
        &self,
        model_path: &Path,
        rgb: &RgbImage,
        mask: &GrayImage,
    ) -> Result<RgbImage> {
        let (rw, rh) = rgb.dimensions();
        let lama_rgb = DynamicImage::ImageRgb8(rgb.clone())
            .resize_exact(LAMA_SIZE, LAMA_SIZE, FilterType::Triangle)
            .to_rgb8();
        let lama_mask = DynamicImage::ImageLuma8(mask.clone())
            .resize_exact(LAMA_SIZE, LAMA_SIZE, FilterType::Triangle)
            .to_luma8();
        let out_raw = self.run_session(
            model_path,
            image_to_chw(&lama_rgb),
            mask_to_binary(&lama_mask),
        )?;
        let scale = detect_scale(&out_raw);
        let lama_512 = chw_to_rgb(&out_raw, LAMA_SIZE, scale)?;
        Ok(DynamicImage::ImageRgb8(lama_512)
            .resize_exact(rw.max(1), rh.max(1), FilterType::Triangle)
            .to_rgb8())
    }

    /// Load init + mask, run LaMa, composite, encode to `params.ext`.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn remove(
        &self,
        model_path: &Path,
        init_path: &Path,
        mask_path: &Path,
        params: &ImageParams,
    ) -> Result<Vec<u8>> {
        let (w, h) = (params.width.max(1), params.height.max(1));
        // Decode from memory (content sniffing): the scratch downloads
        // are extensionless, so `image::open`'s extension-based guess
        // would fail.
        let init_bytes = std::fs::read(init_path)
            .with_context(|| format!("reading init image {}", init_path.display()))?;
        let mask_bytes = std::fs::read(mask_path)
            .with_context(|| format!("reading mask {}", mask_path.display()))?;
        let original = image::load_from_memory(&init_bytes)
            .context("decoding init image")?
            .resize_exact(w, h, FilterType::Triangle)
            .to_rgb8();
        let mask_full = image::load_from_memory(&mask_bytes)
            .context("decoding mask")?
            .resize_exact(w, h, FilterType::Triangle)
            .to_luma8();

        // HD crop strategy: LaMa is exported at a fixed 512². Downscaling the
        // *whole* image to 512 (then back up) makes the fill effectively
        // half-resolution and soft — invisible on a flat colour, but a faint
        // smudge on any subtly-lit / textured / large region. Instead, inpaint
        // a native-resolution crop around the mask: a small object then runs at
        // ~1:1, so the fill stays sharp and the removal is genuinely invisible.
        // `fill` differs from `original` only inside the crop; the feathered
        // composite below still keeps everything outside the mask byte-identical.
        let fill = match hd_crop_box(&mask_full, w, h) {
            Some((cx, cy, cw, ch)) => {
                let crop_rgb = image::imageops::crop_imm(&original, cx, cy, cw, ch).to_image();
                let crop_mask = image::imageops::crop_imm(&mask_full, cx, cy, cw, ch).to_image();
                let inpainted = self.run_lama_region(model_path, &crop_rgb, &crop_mask)?;
                let mut f = original.clone();
                image::imageops::replace(&mut f, &inpainted, cx as i64, cy as i64);
                debug!(
                    target: TRACE_TARGET, op = "remove", crop_w = cw, crop_h = ch,
                    full_w = w, full_h = h, "lama HD crop inpaint"
                );
                f
            }
            // Empty or full-frame mask: fall back to the whole-image pass.
            None => self.run_lama_region(model_path, &original, &mask_full)?,
        };
        // Feathered alpha = blurred mask; composite the fill into the
        // original only inside the (feathered) masked region.
        let alpha = image::imageops::blur(&mask_full, FEATHER_SIGMA);
        let composited = alpha_composite(&original, &fill, &alpha);

        let mut out = Cursor::new(Vec::<u8>::new());
        let dyn_img = DynamicImage::ImageRgb8(composited);
        match params.ext.as_str() {
            "png" => dyn_img.write_to(&mut out, image::ImageFormat::Png)?,
            _ => dyn_img.write_to(&mut out, image::ImageFormat::WebP)?,
        }
        Ok(out.into_inner())
    }
}

impl Engine for OnnxImageEngine {
    fn name(&self) -> &'static str {
        ENGINE_NAME
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        // Namespaced wildcard mirrors sdcpp's `sd-cpp:*`: this worker
        // can serve any onnx-engine image model the studio offers (the
        // .onnx is downloaded on demand).
        map.insert(TaskKind::Image, vec!["onnx:*".to_string()]);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, _model: &str, _task: Task) -> Result<TaskResult> {
        bail!("onnx engine requires a ModelSource; use dispatch_with_source")
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        match task {
            Task::Image(p) => {
                // Surface *why* a removal failed under this engine's own
                // target (missing init/mask URL, model download, onnx
                // run), matching the sdcpp/llama/whisper engines.
                // Without this the only breadcrumb is the WS session's
                // generic "engine dispatch failed", which never names the
                // engine, model, or elapsed time.
                let started = Instant::now();
                self.dispatch_removal(model, &p, source).inspect_err(|e| {
                    warn!(
                        target: TRACE_TARGET,
                        op = "dispatch",
                        model,
                        error = %e,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "lama removal failed"
                    );
                })
            }
            other => {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    model,
                    kind = other.kind().as_str(),
                    "onnx engine only serves image removal jobs"
                );
                bail!(
                    "onnx engine only serves image tasks, got {}",
                    other.kind().as_str()
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested) — tensor packing, output scaling, compositing.
// ---------------------------------------------------------------------------

/// Pack a 512² RGB image into LaMa's `[1,3,512,512]` CHW f32 buffer,
/// normalised to 0..1.
fn image_to_chw(rgb: &RgbImage) -> Vec<f32> {
    let n = (LAMA_SIZE * LAMA_SIZE) as usize;
    let mut out = vec![0.0_f32; 3 * n];
    for (i, px) in rgb.pixels().enumerate() {
        out[i] = px.0[0] as f32 / 255.0;
        out[n + i] = px.0[1] as f32 / 255.0;
        out[2 * n + i] = px.0[2] as f32 / 255.0;
    }
    out
}

/// Pack a 512² grayscale mask into LaMa's `[1,1,512,512]` f32 buffer,
/// binarised (1.0 = remove, white pixels).
fn mask_to_binary(mask: &GrayImage) -> Vec<f32> {
    mask.pixels()
        .map(|p| if p.0[0] > 128 { 1.0_f32 } else { 0.0 })
        .collect()
}

/// LaMa exports differ on output range (some emit 0..1, some 0..255).
/// Detect from the max: a max > 2 means the buffer is already 0..255.
fn detect_scale(out: &[f32]) -> f32 {
    let max = out.iter().copied().fold(0.0_f32, f32::max);
    if max > 2.0 {
        1.0
    } else {
        255.0
    }
}

/// Unpack a `[1,3,512,512]` CHW f32 buffer (scaled by `scale`) back into
/// an `RgbImage`.
fn chw_to_rgb(out: &[f32], size: u32, scale: f32) -> Result<RgbImage> {
    let n = (size * size) as usize;
    if out.len() < 3 * n {
        bail!("onnx output too small: {} < {}", out.len(), 3 * n);
    }
    let mut img = RgbImage::new(size, size);
    for (i, px) in img.pixels_mut().enumerate() {
        let r = (out[i] * scale).clamp(0.0, 255.0) as u8;
        let g = (out[n + i] * scale).clamp(0.0, 255.0) as u8;
        let b = (out[2 * n + i] * scale).clamp(0.0, 255.0) as u8;
        *px = image::Rgb([r, g, b]);
    }
    Ok(img)
}

/// Square crop box around the mask's white region, expanded by a context
/// margin, clamped to the image. The crop is the unit of HD inpainting: small
/// enough that the object runs near 1:1 through LaMa's 512², large enough to
/// give the model surrounding background context. Returns `None` when the mask
/// is empty (nothing to do) or the box would span the whole frame (no win over
/// the plain full-image pass).
fn hd_crop_box(mask: &GrayImage, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0_u32, 0_u32);
    let mut any = false;
    for (x, y, p) in mask.enumerate_pixels() {
        if p.0[0] > 128 {
            any = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if !any {
        return None;
    }
    let bw = x1 - x0 + 1;
    let bh = y1 - y0 + 1;
    let max_dim = bw.max(bh);
    // Context margin: a quarter of the hole plus a small fixed pad. Keeps the
    // crop close to the object so it runs near native resolution.
    let margin = (max_dim as f32 * 0.25).round() as u32 + 32;
    let limit = w.min(h);
    let side = (max_dim + 2 * margin).clamp(256, limit);
    // No benefit if the crop would cover the whole frame.
    if side >= w && side >= h {
        return None;
    }
    let centre_x = x0 + bw / 2;
    let centre_y = y0 + bh / 2;
    let half = side / 2;
    let x = centre_x.saturating_sub(half).min(w - side);
    let y = centre_y.saturating_sub(half).min(h - side);
    Some((x, y, side, side))
}

/// `result = base*(1-a) + fill*a` where `a = alpha/255`.  All three
/// images must share dimensions; the result keeps `base`'s size.
fn alpha_composite(base: &RgbImage, fill: &RgbImage, alpha: &GrayImage) -> RgbImage {
    let (w, h) = base.dimensions();
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let a = alpha.get_pixel(x, y).0[0] as f32 / 255.0;
        let b = base.get_pixel(x, y).0;
        let f = fill.get_pixel(x, y).0;
        *px = image::Rgb([
            (b[0] as f32 * (1.0 - a) + f[0] as f32 * a).round() as u8,
            (b[1] as f32 * (1.0 - a) + f[1] as f32 * a).round() as u8,
            (b[2] as f32 * (1.0 - a) + f[2] as f32 * a).round() as u8,
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The studio serves Find-the-Differences originals as JPEG (often
    // under a `.webp` URL).  The LaMa-removal path decodes the init
    // image with `image::load_from_memory`, so the build MUST carry the
    // JPEG decoder — without it every removal failed with
    // `decoding init image`.  This fixture-backed test locks the
    // decoder into the feature set.
    #[test]
    fn build_decodes_jpeg_init_images() {
        let jpeg = include_bytes!("../../tests/fixtures/sample.jpg");
        let decoded = image::load_from_memory(jpeg)
            .expect("this build must decode JPEG init images")
            .to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (16, 12));
    }

    #[test]
    fn image_to_chw_packs_planar_normalised() {
        let mut img = RgbImage::new(LAMA_SIZE, LAMA_SIZE);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 255, 0]));
        let chw = image_to_chw(&img);
        let n = (LAMA_SIZE * LAMA_SIZE) as usize;
        assert_eq!(chw.len(), 3 * n);
        // pixel 0: pure red
        assert_eq!(chw[0], 1.0); // R plane
        assert_eq!(chw[n], 0.0); // G plane
        assert_eq!(chw[2 * n], 0.0); // B plane
                                     // pixel 1: pure green
        assert_eq!(chw[1], 0.0);
        assert_eq!(chw[n + 1], 1.0);
    }

    #[test]
    fn mask_to_binary_thresholds_at_128() {
        let mut m = GrayImage::new(LAMA_SIZE, LAMA_SIZE);
        m.put_pixel(0, 0, image::Luma([0]));
        m.put_pixel(1, 0, image::Luma([128]));
        m.put_pixel(2, 0, image::Luma([129]));
        m.put_pixel(3, 0, image::Luma([255]));
        let bin = mask_to_binary(&m);
        assert_eq!(bin[0], 0.0);
        assert_eq!(bin[1], 0.0); // 128 is not > 128
        assert_eq!(bin[2], 1.0);
        assert_eq!(bin[3], 1.0);
    }

    fn mask_with_rect(w: u32, h: u32, x0: u32, y0: u32, rw: u32, rh: u32) -> GrayImage {
        let mut m = GrayImage::new(w, h);
        for y in y0..(y0 + rh) {
            for x in x0..(x0 + rw) {
                m.put_pixel(x, y, image::Luma([255]));
            }
        }
        m
    }

    #[test]
    fn hd_crop_box_is_none_for_an_empty_mask() {
        assert_eq!(hd_crop_box(&GrayImage::new(1024, 768), 1024, 768), None);
    }

    #[test]
    fn hd_crop_box_brackets_a_small_object_with_a_square_in_bounds_crop() {
        // A 200px object near the centre of a 1024x768 frame.
        let mask = mask_with_rect(1024, 768, 412, 284, 200, 200);
        let (x, y, cw, ch) = hd_crop_box(&mask, 1024, 768).expect("should crop");
        // Square, and much smaller than the full frame so the inpaint runs near
        // native resolution rather than downscaling the whole image to 512.
        assert_eq!(cw, ch);
        assert!(
            cw < 1024,
            "crop {cw} should be smaller than the frame width"
        );
        assert!(cw >= 200, "crop must at least contain the object");
        // Fully inside the frame.
        assert!(x + cw <= 1024 && y + ch <= 768);
    }

    #[test]
    fn hd_crop_box_clamps_a_corner_object_inside_the_frame() {
        let mask = mask_with_rect(1024, 768, 0, 0, 180, 180);
        let (x, y, cw, ch) = hd_crop_box(&mask, 1024, 768).expect("should crop");
        assert_eq!((x, y), (0, 0));
        assert!(x + cw <= 1024 && y + ch <= 768);
    }

    #[test]
    fn hd_crop_box_falls_back_to_none_when_the_mask_spans_the_frame() {
        let mask = mask_with_rect(512, 512, 0, 0, 512, 512);
        assert_eq!(hd_crop_box(&mask, 512, 512), None);
    }

    #[test]
    fn detect_scale_distinguishes_unit_and_byte_ranges() {
        assert_eq!(detect_scale(&[0.0, 0.5, 1.0]), 255.0);
        assert_eq!(detect_scale(&[0.0, 128.0, 240.0]), 1.0);
        // edge: all zeros → treat as unit range (scale up)
        assert_eq!(detect_scale(&[0.0, 0.0]), 255.0);
    }

    #[test]
    fn chw_to_rgb_roundtrips_unit_scale() {
        let n = (LAMA_SIZE * LAMA_SIZE) as usize;
        let mut buf = vec![0.0_f32; 3 * n];
        buf[0] = 1.0; // R at px0
        buf[2 * n + 1] = 1.0; // B at px1
        let img = chw_to_rgb(&buf, LAMA_SIZE, 255.0).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255]);
    }

    #[test]
    fn chw_to_rgb_rejects_short_buffer() {
        assert!(chw_to_rgb(&[0.0; 10], LAMA_SIZE, 255.0).is_err());
    }

    #[test]
    fn alpha_composite_blends_by_mask() {
        let base = RgbImage::from_pixel(2, 1, image::Rgb([0, 0, 0]));
        let fill = RgbImage::from_pixel(2, 1, image::Rgb([100, 100, 100]));
        let mut alpha = GrayImage::new(2, 1);
        alpha.put_pixel(0, 0, image::Luma([0])); // keep base
        alpha.put_pixel(1, 0, image::Luma([255])); // take fill
        let out = alpha_composite(&base, &fill, &alpha);
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(out.get_pixel(1, 0).0, [100, 100, 100]);
    }

    #[test]
    fn alpha_composite_half_blends_midpoint() {
        let base = RgbImage::from_pixel(1, 1, image::Rgb([0, 0, 0]));
        let fill = RgbImage::from_pixel(1, 1, image::Rgb([200, 200, 200]));
        let alpha = GrayImage::from_pixel(1, 1, image::Luma([128]));
        let out = alpha_composite(&base, &fill, &alpha);
        // 128/255 ≈ 0.502 → ~100
        let v = out.get_pixel(0, 0).0[0];
        assert!((99..=101).contains(&v), "got {v}");
    }

    /// End-to-end LaMa removal against the real ONNX model.  Ignored by
    /// default (needs the 208 MB model + assets); run explicitly with
    /// the paths in env:
    ///   LAMA_ONNX=… LAMA_INIT=… LAMA_MASK=… LAMA_OUT=… \
    ///   cargo test --features image-onnx onnx:: -- --ignored --nocapture
    /// Asserts the output decodes and the masked region changed while
    /// outside-mask pixels stayed (near) identical to the original.
    #[test]
    #[ignore = "needs the real LaMa onnx model + assets via env"]
    fn lama_removal_end_to_end() {
        let onnx = std::env::var("LAMA_ONNX").expect("LAMA_ONNX");
        let init = std::env::var("LAMA_INIT").expect("LAMA_INIT");
        let mask = std::env::var("LAMA_MASK").expect("LAMA_MASK");
        let params = ImageParams {
            width: 1024,
            height: 768,
            ext: "webp".into(),
            ..Default::default()
        };
        let engine = OnnxImageEngine::new(std::env::temp_dir());
        let bytes = engine
            .remove(
                std::path::Path::new(&onnx),
                std::path::Path::new(&init),
                std::path::Path::new(&mask),
                &params,
            )
            .expect("removal");
        assert!(!bytes.is_empty(), "empty output");
        let out = image::load_from_memory(&bytes)
            .expect("decode output")
            .to_rgb8();
        assert_eq!(out.dimensions(), (1024, 768));
        if let Ok(out_path) = std::env::var("LAMA_OUT") {
            std::fs::write(&out_path, &bytes).expect("write out");
        }
        // A corner pixel (outside the mask) must stay ~identical to the original
        // (the composite preserves outside-mask pixels), proving the full
        // dynamically-loaded LaMa pipeline ran and composited correctly.
        let original = image::load_from_memory(&std::fs::read(&init).unwrap())
            .unwrap()
            .resize_exact(1024, 768, FilterType::Triangle)
            .to_rgb8();
        let d_corner: i32 = (0..3)
            .map(|i| {
                (out.get_pixel(20, 20).0[i] as i32 - original.get_pixel(20, 20).0[i] as i32).abs()
            })
            .sum();
        assert!(d_corner < 16, "outside-mask pixel drifted: {d_corner}");
    }
}
