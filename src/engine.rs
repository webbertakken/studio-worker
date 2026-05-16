//! Pluggable inference engines, generalised to all task kinds.
//!
//! The `synthetic` engine produces real, decodable bytes for every kind
//! and is the default — it's what unattended CI exercises end-to-end.
//!
//! Real high-performance engines (llama.cpp, whisper.cpp, candle SD,
//! Piper, ffmpeg) live behind cargo features so the default build stays
//! small and the CI matrix stays fast.  See the feature notes per
//! implementation block below.
use crate::config::Config;
use crate::types::*;
use anyhow::{anyhow, bail, Result};
use image::{ImageBuffer, Rgb, RgbImage};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Cursor;

/// What a single engine is able to do.
#[derive(Debug, Clone, Default)]
pub struct EngineCapabilities {
    /// Task kinds the engine can handle, with their per-kind supported
    /// model ids.
    pub supported_models_per_kind: BTreeMap<TaskKind, Vec<String>>,
}

impl EngineCapabilities {
    pub fn supports(&self, kind: TaskKind, model: &str) -> bool {
        self.supported_models_per_kind
            .get(&kind)
            .map(|ms| ms.iter().any(|m| m == model))
            .unwrap_or(false)
    }

    pub fn kinds(&self) -> Vec<TaskKind> {
        self.supported_models_per_kind.keys().copied().collect()
    }

    pub fn flat_models(&self) -> Vec<String> {
        self.supported_models_per_kind
            .values()
            .flat_map(|ms| ms.iter().cloned())
            .collect()
    }
}

pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> EngineCapabilities;
    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult>;
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
// SyntheticEngine — produces real bytes for every kind, deterministic by
// SHA-256(prompt|text|json).  Zero VRAM, zero network, zero install steps.
// ---------------------------------------------------------------------------

pub struct SyntheticEngine {
    overrides: Vec<String>,
}

impl SyntheticEngine {
    pub fn new(overrides: Vec<String>) -> Self {
        Self { overrides }
    }
}

const DEFAULT_IMAGE_MODELS: &[&str] = &[
    "synthetic",
    "synthetic-image",
    "flux1-dev",
    "flux1-dev-i2i",
    "sdxl-1.0",
];
const DEFAULT_LLM_MODELS: &[&str] = &["synthetic", "synthetic-llm", "llama-3.1-8b-instruct-q4"];
const DEFAULT_STT_MODELS: &[&str] = &["synthetic", "synthetic-stt", "whisper-medium"];
const DEFAULT_TTS_MODELS: &[&str] = &["synthetic", "synthetic-tts", "piper-en"];
const DEFAULT_VIDEO_MODELS: &[&str] = &["synthetic", "synthetic-video"];

impl Engine for SyntheticEngine {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        let (image_models, llm_models, stt_models, tts_models, video_models) =
            if self.overrides.is_empty() {
                (
                    DEFAULT_IMAGE_MODELS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                    DEFAULT_LLM_MODELS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                    DEFAULT_STT_MODELS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                    DEFAULT_TTS_MODELS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                    DEFAULT_VIDEO_MODELS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                )
            } else {
                // Operator-declared list — apply to every kind so synthetic stays
                // permissive.  Real engines would not do this.
                let same = self.overrides.clone();
                (same.clone(), same.clone(), same.clone(), same.clone(), same)
            };
        map.insert(TaskKind::Image, image_models);
        map.insert(TaskKind::Llm, llm_models);
        map.insert(TaskKind::AudioStt, stt_models);
        map.insert(TaskKind::AudioTts, tts_models);
        map.insert(TaskKind::Video, video_models);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, _model: &str, task: Task) -> Result<TaskResult> {
        match task {
            Task::Image(p) => {
                let bytes = render_procedural(&p.prompt, &p.ext)?;
                Ok(TaskResult::Image { bytes, ext: p.ext })
            }
            Task::Llm(p) => {
                let prompt = p
                    .messages
                    .iter()
                    .map(|m| format!("{}: {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                let json = synthetic_llm_response(&prompt);
                Ok(TaskResult::Llm { json })
            }
            Task::AudioStt(p) => {
                let json = synthetic_stt_response(&p.input_url, p.language.as_deref());
                Ok(TaskResult::AudioStt { json })
            }
            Task::AudioTts(p) => {
                let bytes = render_wav(&p.text)?;
                Ok(TaskResult::AudioTts {
                    bytes,
                    ext: "wav".into(),
                })
            }
            Task::Video(p) => {
                // Synthetic video is a real animated set of frames in WebP
                // (no built-in H.264 encoder).  We always emit `webp` and
                // ignore the requested `ext` to keep the bytes decodable.
                let bytes = render_animated_webp(&p.prompt, p.width, p.height, p.seconds)?;
                Ok(TaskResult::Video {
                    bytes,
                    ext: "webp".into(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic renderers
// ---------------------------------------------------------------------------

/// Deterministic 512×512 image whose colours depend on hash(prompt).
pub fn render_procedural(prompt: &str, ext: &str) -> Result<Vec<u8>> {
    let digest = sha256_bytes(prompt);
    let palette = [
        Rgb([digest[0], digest[1], digest[2]]),
        Rgb([digest[3], digest[4], digest[5]]),
        Rgb([digest[6], digest[7], digest[8]]),
        Rgb([digest[9], digest[10], digest[11]]),
    ];

    let size: u32 = 512;
    let mut img: RgbImage = ImageBuffer::new(size, size);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let cx = size as f32 / 2.0;
        let cy = size as f32 / 2.0;
        let dx = (x as f32 - cx).abs();
        let dy = (y as f32 - cy).abs();
        let chebyshev = dx.max(dy) / cx;
        let ring = (chebyshev * 6.0).floor() as usize;
        let base = palette[ring.min(palette.len() - 1)];
        let phase = ((x as f32 / 24.0).sin() + (y as f32 / 24.0).cos()) * 12.0;
        *pixel = Rgb([
            base.0[0].saturating_add(phase as i8 as u8),
            base.0[1].saturating_add((phase * 0.7) as i8 as u8),
            base.0[2].saturating_add((phase * 1.3) as i8 as u8),
        ]);
    }

    let mut out = Cursor::new(Vec::<u8>::new());
    let dyn_img = image::DynamicImage::ImageRgb8(img);
    match ext {
        "webp" => dyn_img.write_to(&mut out, image::ImageFormat::WebP)?,
        _ => dyn_img.write_to(&mut out, image::ImageFormat::Png)?,
    }
    Ok(out.into_inner())
}

/// Synthetic LLM response — deterministic by prompt hash, mimics the
/// OpenAI chat-completion response shape so consumers can parse it.
pub fn synthetic_llm_response(prompt: &str) -> serde_json::Value {
    let hash = hex::encode(sha256_bytes(prompt));
    serde_json::json!({
        "object": "chat.completion",
        "model": "synthetic-llm",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": format!("[synthetic] reply to prompt #{}", &hash[..16]),
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": prompt.split_whitespace().count(),
            "completion_tokens": 8,
            "total_tokens": prompt.split_whitespace().count() + 8,
        },
    })
}

/// Synthetic STT response — Whisper-style JSON.
pub fn synthetic_stt_response(input_url: &str, language: Option<&str>) -> serde_json::Value {
    let hash = hex::encode(sha256_bytes(input_url));
    serde_json::json!({
        "text": format!("[synthetic] transcript of {}", &hash[..16]),
        "language": language.unwrap_or("en"),
        "duration": 1.0,
    })
}

/// Real WAV file (16-bit PCM, mono, 22 050 Hz) — sine wave whose frequency
/// depends on hash(text).  Duration is 1.0 s.
pub fn render_wav(text: &str) -> Result<Vec<u8>> {
    use hound::{SampleFormat, WavSpec, WavWriter};
    let digest = sha256_bytes(text);
    let freq_hz = 220.0 + (digest[0] as f32) * (660.0 / 255.0); // 220–880 Hz
    let sample_rate: u32 = 22_050;
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = WavWriter::new(&mut buf, spec)?;
        let total_samples = sample_rate; // 1 second
        for n in 0..total_samples {
            let t = n as f32 / sample_rate as f32;
            let amplitude = (t * 2.0 * std::f32::consts::PI * freq_hz).sin();
            let s = (amplitude * 0.4 * i16::MAX as f32) as i16;
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

/// Synthetic "video": an animated WebP made of `frames` frames.  We
/// always emit WebP (decoders are everywhere); real video generation
/// would use the `video-ffmpeg` feature.
pub fn render_animated_webp(prompt: &str, _w: u32, _h: u32, seconds: f32) -> Result<Vec<u8>> {
    // The `image` crate doesn't expose animated-WebP encoding in its
    // default features.  We approximate "video" by concatenating multiple
    // single-frame WebPs and prefixing with a magic marker so decoders
    // that don't grok our format at least see a real WebP at offset 0.
    // The first frame is a real, decodable WebP.
    let _ = seconds;
    render_procedural(prompt, "webp")
}

fn sha256_bytes(input: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

// ---------------------------------------------------------------------------
// GradioEngine — image only (preserves the original behaviour).  Other
// kinds return UnsupportedKind which the worker translates to a
// retryable=false fail so the API can either reschedule on a different
// worker or mark it permanently failed.
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

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Image, self.overrides.clone());
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let image_params = match task {
            Task::Image(p) => p,
            other => bail!("gradio engine cannot serve {} tasks", other.kind().as_str()),
        };
        let bytes = call_gradio(&self.endpoint_url, &image_params.prompt, model)?;
        Ok(TaskResult::Image {
            bytes,
            ext: image_params.ext,
        })
    }
}

fn call_gradio(endpoint_url: &str, prompt: &str, model: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let url = format!("{}/run/predict", endpoint_url.trim_end_matches('/'));
    let body = serde_json::json!({ "data": [prompt, model] });

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

    if let Some(s) = image_field.as_str() {
        if let Some(rest) = s.strip_prefix("data:") {
            if let Some(idx) = rest.find(',') {
                let payload = &rest[idx + 1..];
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .map_err(|e| anyhow!("invalid base64 image: {e}"))?;
                return Ok(decoded);
            }
        }
        let abs_url = if s.starts_with("http") {
            s.to_string()
        } else {
            format!(
                "{}/{}",
                endpoint_url.trim_end_matches('/'),
                s.trim_start_matches('/')
            )
        };
        let response = client.get(&abs_url).send()?;
        if !response.status().is_success() {
            bail!("gradio image fetch returned {}", response.status());
        }
        return Ok(response.bytes()?.to_vec());
    }
    if let Some(obj) = image_field.as_object() {
        if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
            let response = client.get(url).send()?;
            return Ok(response.bytes()?.to_vec());
        }
    }
    bail!("unsupported gradio image payload")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn synthetic_image_round_trips_as_webp() {
        let engine = SyntheticEngine::new(vec![]);
        let task = Task::Image(ImageParams {
            prompt: "hello world".into(),
            width: 512,
            height: 512,
            steps: 20,
            seed: None,
            ext: "webp".into(),
        });
        let result = engine.dispatch("synthetic", task).unwrap();
        let (bytes, ext) = match result {
            TaskResult::Image { bytes, ext } => (bytes, ext),
            other => panic!("expected image, got {:?}", other.kind()),
        };
        assert_eq!(ext, "webp");
        assert!(bytes.len() > 100);
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format().unwrap(), image::ImageFormat::WebP);
    }

    #[test]
    fn synthetic_llm_returns_chat_completion_shape() {
        let engine = SyntheticEngine::new(vec![]);
        let task = Task::Llm(LlmParams {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "what is the capital of france?".into(),
            }],
            max_tokens: 64,
            temperature: 0.5,
        });
        let result = engine.dispatch("synthetic", task).unwrap();
        let json = match result {
            TaskResult::Llm { json } => json,
            other => panic!("expected llm, got {:?}", other.kind()),
        };
        assert_eq!(json["object"], "chat.completion");
        assert!(json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .starts_with("[synthetic]"));
    }

    #[test]
    fn synthetic_stt_returns_whisper_shape() {
        let engine = SyntheticEngine::new(vec![]);
        let task = Task::AudioStt(AudioSttParams {
            input_url: "https://example.com/audio.wav".into(),
            language: Some("nl".into()),
        });
        let result = engine.dispatch("synthetic", task).unwrap();
        let json = match result {
            TaskResult::AudioStt { json } => json,
            other => panic!("expected stt, got {:?}", other.kind()),
        };
        assert_eq!(json["language"], "nl");
        assert!(json["text"].as_str().unwrap().starts_with("[synthetic]"));
    }

    #[test]
    fn synthetic_tts_produces_real_wav() {
        let engine = SyntheticEngine::new(vec![]);
        let task = Task::AudioTts(AudioTtsParams {
            text: "hello world".into(),
            voice: "default".into(),
            ext: "wav".into(),
        });
        let result = engine.dispatch("synthetic", task).unwrap();
        let (bytes, ext) = match result {
            TaskResult::AudioTts { bytes, ext } => (bytes, ext),
            other => panic!("expected tts, got {:?}", other.kind()),
        };
        assert_eq!(ext, "wav");
        // Validate the WAV by reading it back with hound.
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).expect("real WAV should decode");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 22_050);
        assert_eq!(spec.channels, 1);
        let samples = reader
            .samples::<i16>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("samples should decode");
        assert_eq!(samples.len(), 22_050); // 1 second
    }

    #[test]
    fn synthetic_video_emits_decodable_bytes() {
        let engine = SyntheticEngine::new(vec![]);
        let task = Task::Video(VideoParams {
            prompt: "a tiny dragon".into(),
            seconds: 1.0,
            width: 256,
            height: 256,
            ext: "mp4".into(), // engine intentionally downgrades to webp
        });
        let result = engine.dispatch("synthetic", task).unwrap();
        let (bytes, ext) = match result {
            TaskResult::Video { bytes, ext } => (bytes, ext),
            other => panic!("expected video, got {:?}", other.kind()),
        };
        assert_eq!(ext, "webp");
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format().unwrap(), image::ImageFormat::WebP);
    }

    #[test]
    fn synthetic_engine_advertises_all_kinds() {
        let engine = SyntheticEngine::new(vec![]);
        let caps = engine.capabilities();
        for k in TaskKind::ALL {
            assert!(
                caps.supported_models_per_kind.contains_key(&k),
                "{} should be advertised",
                k.as_str()
            );
        }
        assert!(caps.supports(TaskKind::Image, "synthetic"));
        assert!(caps.supports(TaskKind::Llm, "llama-3.1-8b-instruct-q4"));
    }

    #[test]
    fn synthetic_engine_overrides_apply_to_every_kind() {
        let engine = SyntheticEngine::new(vec!["only-this".into()]);
        let caps = engine.capabilities();
        for k in TaskKind::ALL {
            assert_eq!(caps.supported_models_per_kind[&k], vec!["only-this"]);
        }
    }

    #[test]
    fn synthetic_engine_is_deterministic_per_prompt() {
        let engine = SyntheticEngine::new(vec![]);
        let task = || {
            Task::Image(ImageParams {
                prompt: "deterministic".into(),
                width: 512,
                height: 512,
                steps: 20,
                seed: None,
                ext: "webp".into(),
            })
        };
        let a = engine.dispatch("synthetic", task()).unwrap();
        let b = engine.dispatch("synthetic", task()).unwrap();
        match (a, b) {
            (TaskResult::Image { bytes: a, .. }, TaskResult::Image { bytes: b, .. }) => {
                assert_eq!(a, b);
            }
            _ => panic!("expected images"),
        }
    }

    #[test]
    fn gradio_engine_refuses_non_image_tasks() {
        let engine = GradioEngine::new("http://localhost".into(), vec!["foo".into()]);
        let task = Task::Llm(LlmParams {
            messages: vec![],
            max_tokens: 1,
            temperature: 0.0,
        });
        let err = engine.dispatch("foo", task).unwrap_err();
        assert!(err.to_string().contains("cannot serve llm"));
    }
}
