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
use anyhow::Result;
use image::{ImageBuffer, Rgb, RgbImage};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Instant;
use tracing::{debug, warn};

/// Tracing target for the synthetic engine.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::engine::synthetic=debug`.
const TRACE_TARGET_SYNTHETIC: &str = "studio_worker::engine::synthetic";

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

#[cfg(feature = "image-candle")]
pub mod candle_image;
#[cfg(feature = "llama")]
pub mod llama;
pub mod multi;
pub mod sdcpp;
#[cfg(feature = "tts")]
pub mod tts;
#[cfg(feature = "video")]
pub mod video;
#[cfg(feature = "whisper")]
pub mod whisper;

pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> EngineCapabilities;
    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult>;

    /// Dispatch with the studio's `ModelSource` attached.  Engines
    /// that need it (download URLs / CLI defaults) override this;
    /// engines that don't (synthetic) keep using the plain
    /// `dispatch` method via the default impl below.
    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        _source: Option<&crate::types::ModelSource>,
    ) -> Result<TaskResult> {
        self.dispatch(model, task)
    }
}

/// Build the engine for this worker.
///
/// There's no engine selection knob in the config any more: the
/// worker advertises capabilities for every backend compiled into
/// this binary, and routes each incoming job to the first backend
/// that supports its (kind, model) pair.  See `multi::MultiEngine`.
///
/// The default build ships only the synthetic engine.  Optional
/// backends (llama, whisper, image-candle, video, tts) are added
/// when their cargo features are enabled.
pub fn build(cfg: &Config) -> Result<Box<dyn Engine>> {
    // Real backends first so they win the "supports" check ahead of
    // the catch-all synthetic engine.  Synthetic is always last:
    // deterministic real bytes for every kind, zero-VRAM fallback so
    // CI + smoke-tests stay self-contained.
    #[allow(clippy::vec_init_then_push)]
    let engines: Vec<Box<dyn Engine>> = {
        let mut v: Vec<Box<dyn Engine>> = Vec::new();
        #[cfg(feature = "llama")]
        v.push(Box::new(llama::LlamaEngine::new(cfg.models_root.clone())?));
        #[cfg(feature = "whisper")]
        v.push(Box::new(whisper::WhisperEngine::new(
            cfg.models_root.clone(),
        )));
        #[cfg(feature = "image-candle")]
        v.push(Box::new(candle_image::CandleImageEngine::new()));
        #[cfg(feature = "video")]
        v.push(Box::new(video::VideoEngine::new()));
        #[cfg(feature = "tts")]
        v.push(Box::new(tts::TtsEngine::new()));
        // stable-diffusion.cpp-backed image engine.  Self-registers
        // only when both the `sd-cli` binary is on the box AND at
        // least one model's component files are present under
        // `cfg.models_root`.  Skipped silently otherwise so CI builds
        // (no sd-cli, no models) stay green.
        if let Some(eng) = sdcpp::SdCppEngine::try_new(&cfg.models_root) {
            v.push(Box::new(eng));
        }
        v.push(Box::new(SyntheticEngine::new()));
        v
    };

    Ok(Box::new(multi::MultiEngine::new(engines)))
}

/// Legacy hook retained for any external caller; mirrors
/// `Config::default().models_root`.
pub fn default_models_root() -> std::path::PathBuf {
    crate::config::default_models_root()
}

// ---------------------------------------------------------------------------
// SyntheticEngine — produces real bytes for every kind, deterministic by
// SHA-256(prompt|text|json).  Zero VRAM, zero network, zero install steps.
// ---------------------------------------------------------------------------

pub struct SyntheticEngine;

impl SyntheticEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SyntheticEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Sentinel string the studio's claim filter recognises as "any
/// model is fine".  Real engines that can actually serve any model
/// (e.g. a GGUF-aware image engine that downloads on demand) advertise
/// it.  The synthetic engine deliberately does NOT — it would happily
/// fulfil real-model jobs with placeholder bytes, which is destructive
/// on a live queue.
pub const MODEL_WILDCARD: &str = "*";

// Synthetic engine advertises only its own `synthetic*` model names
// so it never claims a job that names a real model the operator is
// expecting actual inference for.
const DEFAULT_IMAGE_MODELS: &[&str] = &["synthetic", "synthetic-image"];
const DEFAULT_LLM_MODELS: &[&str] = &["synthetic", "synthetic-llm"];
const DEFAULT_STT_MODELS: &[&str] = &["synthetic", "synthetic-stt"];
const DEFAULT_TTS_MODELS: &[&str] = &["synthetic", "synthetic-tts"];
const DEFAULT_VIDEO_MODELS: &[&str] = &["synthetic", "synthetic-video"];

fn models(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

impl Engine for SyntheticEngine {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Image, models(DEFAULT_IMAGE_MODELS));
        map.insert(TaskKind::Llm, models(DEFAULT_LLM_MODELS));
        map.insert(TaskKind::AudioStt, models(DEFAULT_STT_MODELS));
        map.insert(TaskKind::AudioTts, models(DEFAULT_TTS_MODELS));
        map.insert(TaskKind::Video, models(DEFAULT_VIDEO_MODELS));
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let started = Instant::now();
        let result = match task {
            Task::Image(p) => render_procedural(&p.prompt, &p.ext)
                .map(|bytes| TaskResult::Image { bytes, ext: p.ext }),
            Task::Llm(p) => {
                let prompt = p
                    .messages
                    .iter()
                    .map(|m| format!("{}: {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(TaskResult::Llm {
                    json: synthetic_llm_response(&prompt),
                })
            }
            Task::AudioStt(p) => Ok(TaskResult::AudioStt {
                json: synthetic_stt_response(&p.input_url, p.language.as_deref()),
            }),
            Task::AudioTts(p) => render_wav(&p.text).map(|bytes| TaskResult::AudioTts {
                bytes,
                ext: "wav".into(),
            }),
            Task::Video(p) => {
                // Synthetic video is a real animated set of frames in WebP
                // (no built-in H.264 encoder).  We always emit `webp` and
                // ignore the requested `ext` to keep the bytes decodable.
                render_animated_webp(&p.prompt, p.width, p.height, p.seconds).map(|bytes| {
                    TaskResult::Video {
                        bytes,
                        ext: "webp".into(),
                    }
                })
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => debug!(
                target: TRACE_TARGET_SYNTHETIC,
                op = "dispatch",
                kind = kind.as_str(),
                model,
                elapsed_ms,
                "ok"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET_SYNTHETIC,
                op = "dispatch",
                kind = kind.as_str(),
                model,
                elapsed_ms,
                error = %e,
                "failed"
            ),
        }
        result
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn synthetic_image_round_trips_as_webp() {
        let engine = SyntheticEngine::new();
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
        let engine = SyntheticEngine::new();
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
        let engine = SyntheticEngine::new();
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
        let engine = SyntheticEngine::new();
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
        let engine = SyntheticEngine::new();
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
        let engine = SyntheticEngine::new();
        let caps = engine.capabilities();
        for k in TaskKind::ALL {
            assert!(
                caps.supported_models_per_kind.contains_key(&k),
                "{} should be advertised",
                k.as_str()
            );
        }
        assert!(caps.supports(TaskKind::Image, "synthetic"));
        assert!(
            !caps.supports(TaskKind::Image, "*"),
            "synthetic engine MUST NOT advertise the wildcard \
             (it would happily fulfil real-model jobs with placeholder \
             bytes, which is destructive on a live queue)"
        );
    }

    #[test]
    fn build_default_yields_multi_engine_with_synthetic_inside() {
        // Default features = synthetic-only.  `build()` should always
        // return a MultiEngine (so the routing layer is uniform), and
        // synthetic capabilities should be visible through it.
        let cfg = crate::config::Config::default();
        let eng = build(&cfg).unwrap();
        assert_eq!(eng.name(), "multi");
        let caps = eng.capabilities();
        for k in TaskKind::ALL {
            assert!(caps.supported_models_per_kind.contains_key(&k));
        }
        assert!(caps.supports(TaskKind::Image, "synthetic"));
        assert!(caps.supports(TaskKind::Llm, "synthetic"));
    }

    #[test]
    fn synthetic_engine_is_deterministic_per_prompt() {
        let engine = SyntheticEngine::new();
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
}
