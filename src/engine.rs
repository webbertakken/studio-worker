//! Pluggable inference engines.
//!
//! The `synthetic` engine produces deterministic, real PNG/WEBP images
//! keyed by SHA-256 of the prompt.  It is the default and exercises the
//! full pipeline end-to-end without any GPU.
//!
//! The `gradio` engine targets a user-installed Gradio server on
//! `127.0.0.1` (no cloudflared, no proxy).  It supports any model the
//! local Gradio exposes; the user lists those model ids in
//! `supported_models_override`.
use crate::config::Config;
use anyhow::{anyhow, bail, Result};
use image::{ImageBuffer, Rgb, RgbImage};
use sha2::{Digest, Sha256};
use std::io::Cursor;

pub trait Engine: Send + Sync {
    #[allow(dead_code)] // referenced via diagnostics + future runtime introspection
    fn name(&self) -> &'static str;

    /// The model ids this engine can serve.  Workers refuse to claim jobs
    /// whose `model` isn't in this list.
    fn supported_models(&self) -> Vec<String>;

    /// Generate an image for the given prompt + model, returning the
    /// raw encoded bytes (matching the requested extension).
    fn generate(&self, prompt: &str, model: &str, ext: &str) -> Result<Vec<u8>>;
}

pub fn build(cfg: &Config) -> Result<Box<dyn Engine>> {
    match cfg.engine.as_str() {
        "synthetic" => Ok(Box::new(SyntheticEngine::new(
            cfg.supported_models_override.clone(),
        ))),
        "gradio" => {
            let url = cfg
                .gradio_endpoint_url
                .clone()
                .ok_or_else(|| anyhow!("gradio engine requires gradio_endpoint_url"))?;
            Ok(Box::new(GradioEngine::new(
                url,
                cfg.supported_models_override.clone(),
            )))
        }
        other => bail!("unknown engine: {other}"),
    }
}

// ---------------------------------------------------------------------------
// SyntheticEngine
// ---------------------------------------------------------------------------

pub struct SyntheticEngine {
    overrides: Vec<String>,
}

impl SyntheticEngine {
    pub fn new(overrides: Vec<String>) -> Self {
        Self { overrides }
    }
}

const SYNTHETIC_MODELS: &[&str] = &["synthetic", "flux1-dev", "flux1-dev-i2i", "sdxl-1.0"];

impl Engine for SyntheticEngine {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn supported_models(&self) -> Vec<String> {
        if !self.overrides.is_empty() {
            return self.overrides.clone();
        }
        SYNTHETIC_MODELS.iter().map(|s| (*s).to_string()).collect()
    }

    fn generate(&self, prompt: &str, _model: &str, ext: &str) -> Result<Vec<u8>> {
        let bytes = render_procedural(prompt, ext)?;
        Ok(bytes)
    }
}

/// Produce a deterministic 512x512 image whose colours depend on the
/// SHA-256 of the prompt.  Encodes to PNG or WEBP per `ext`.
pub fn render_procedural(prompt: &str, ext: &str) -> Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let palette = [
        Rgb([digest[0], digest[1], digest[2]]),
        Rgb([digest[3], digest[4], digest[5]]),
        Rgb([digest[6], digest[7], digest[8]]),
        Rgb([digest[9], digest[10], digest[11]]),
    ];

    let size: u32 = 512;
    let mut img: RgbImage = ImageBuffer::new(size, size);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        // Concentric rounded squares using the palette + radial gradient.
        let cx = size as f32 / 2.0;
        let cy = size as f32 / 2.0;
        let dx = (x as f32 - cx).abs();
        let dy = (y as f32 - cy).abs();
        let chebyshev = dx.max(dy) / cx;
        let ring = (chebyshev * 6.0).floor() as usize;
        let base = palette[ring.min(palette.len() - 1)];
        // Sprinkle a wavy texture for visual interest.
        let phase = ((x as f32 / 24.0).sin() + (y as f32 / 24.0).cos()) * 12.0;
        *pixel = Rgb([
            base.0[0].saturating_add(phase as i8 as u8),
            base.0[1].saturating_add((phase * 0.7) as i8 as u8),
            base.0[2].saturating_add((phase * 1.3) as i8 as u8),
        ]);
    }

    let mut out = Cursor::new(Vec::<u8>::new());
    match ext {
        "webp" => {
            let dyn_img = image::DynamicImage::ImageRgb8(img);
            dyn_img.write_to(&mut out, image::ImageFormat::WebP)?;
        }
        _ => {
            let dyn_img = image::DynamicImage::ImageRgb8(img);
            dyn_img.write_to(&mut out, image::ImageFormat::Png)?;
        }
    }
    Ok(out.into_inner())
}

// ---------------------------------------------------------------------------
// GradioEngine
// ---------------------------------------------------------------------------

pub struct GradioEngine {
    pub endpoint_url: String,
    overrides: Vec<String>,
}

impl GradioEngine {
    pub fn new(endpoint_url: String, overrides: Vec<String>) -> Self {
        Self {
            endpoint_url,
            overrides,
        }
    }
}

impl Engine for GradioEngine {
    fn name(&self) -> &'static str {
        "gradio"
    }

    fn supported_models(&self) -> Vec<String> {
        // For Gradio we always require the operator to declare the model
        // ids the local Gradio supports — there's no portable way to
        // probe a Gradio install for "models" generically.
        self.overrides.clone()
    }

    fn generate(&self, prompt: &str, model: &str, ext: &str) -> Result<Vec<u8>> {
        // Hit the local Gradio's POST {endpoint}/api/call/generate-style endpoint.
        // The exact endpoint depends on the user's Gradio app; the worker
        // simply forwards `{ data: [prompt, model] }` and expects an
        // image URL/base64 back.  We do a synchronous call here using the
        // `reqwest` blocking client because the runtime caller wraps us
        // in `spawn_blocking`.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;

        let url = format!("{}/run/predict", self.endpoint_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "data": [prompt, model],
        });

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| anyhow!("gradio request failed: {e}"))?;
        if !response.status().is_success() {
            bail!("gradio returned {}", response.status());
        }

        let parsed: serde_json::Value = response.json()?;
        let image_field = parsed
            .get("data")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("gradio response missing data[0]"))?;
        let bytes = extract_image_bytes(image_field, &client, &self.endpoint_url, ext)?;
        Ok(bytes)
    }
}

fn extract_image_bytes(
    value: &serde_json::Value,
    client: &reqwest::blocking::Client,
    base: &str,
    _ext: &str,
) -> Result<Vec<u8>> {
    use base64::Engine as _;
    if let Some(s) = value.as_str() {
        if let Some(rest) = s.strip_prefix("data:") {
            // data URL
            if let Some(idx) = rest.find(",") {
                let payload = &rest[idx + 1..];
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .map_err(|e| anyhow!("invalid base64 image: {e}"))?;
                return Ok(decoded);
            }
        }
        let url = if s.starts_with("http") {
            s.to_string()
        } else {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                s.trim_start_matches('/')
            )
        };
        let response = client.get(&url).send()?;
        if !response.status().is_success() {
            bail!("gradio image fetch returned {}", response.status());
        }
        return Ok(response.bytes()?.to_vec());
    }
    if let Some(obj) = value.as_object() {
        if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            let response = client.get(url).send()?;
            return Ok(response.bytes()?.to_vec());
        }
    }
    bail!("unsupported gradio image payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn synthetic_engine_produces_valid_webp() {
        let engine = SyntheticEngine::new(vec![]);
        let bytes = engine.generate("hello world", "synthetic", "webp").unwrap();
        assert!(
            bytes.len() > 100,
            "image should be at least 100 bytes, got {}",
            bytes.len()
        );
        // Decode it back to make sure it's a valid WEBP.
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap();
        let format = reader.format().expect("guessable format");
        assert_eq!(format, image::ImageFormat::WebP);
        let img = reader.decode().expect("decode webp");
        assert_eq!(img.width(), 512);
        assert_eq!(img.height(), 512);
    }

    #[test]
    fn synthetic_engine_produces_valid_png() {
        let engine = SyntheticEngine::new(vec![]);
        let bytes = engine
            .generate("another prompt", "synthetic", "png")
            .unwrap();
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format().unwrap(), image::ImageFormat::Png);
    }

    #[test]
    fn synthetic_engine_is_deterministic_per_prompt() {
        let engine = SyntheticEngine::new(vec![]);
        let a = engine
            .generate("deterministic", "synthetic", "webp")
            .unwrap();
        let b = engine
            .generate("deterministic", "synthetic", "webp")
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn synthetic_engine_is_distinct_per_prompt() {
        let engine = SyntheticEngine::new(vec![]);
        let a = engine.generate("alpha", "synthetic", "webp").unwrap();
        let b = engine.generate("beta", "synthetic", "webp").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn synthetic_engine_lists_default_models() {
        let engine = SyntheticEngine::new(vec![]);
        let models = engine.supported_models();
        assert!(models.contains(&"flux1-dev".to_string()));
        assert!(models.contains(&"synthetic".to_string()));
    }

    #[test]
    fn synthetic_engine_overrides_models() {
        let engine = SyntheticEngine::new(vec!["custom-model".to_string()]);
        assert_eq!(engine.supported_models(), vec!["custom-model".to_string()]);
    }
}
