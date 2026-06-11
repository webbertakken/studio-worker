//! Real audio transcription via [`whisper-rs`].
//!
//! Compiled in with `--features whisper`.  Looks for `*.bin` GGML
//! whisper models under `<models_root>/stt/` and exposes their stems as
//! model ids.  For an STT task it fetches the audio at `input_url`,
//! decodes it as WAV (16-bit PCM mono, 16 kHz), and returns a
//! Whisper-shape transcript.
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Tracing target for the whisper engine.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::engine::whisper=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::whisper";

pub struct WhisperEngine {
    models_root: PathBuf,
    cached: Mutex<Option<CachedModel>>,
}

struct CachedModel {
    id: String,
    ctx: Arc<WhisperContext>,
}

impl WhisperEngine {
    pub fn new(models_root: PathBuf) -> Self {
        Self {
            models_root,
            cached: Mutex::new(None),
        }
    }

    fn stt_dir(&self) -> PathBuf {
        self.models_root.join("stt")
    }

    fn list_models(&self) -> Vec<(String, PathBuf)> {
        let dir = self.stt_dir();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("bin") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push((stem.to_string(), p));
                }
            }
        }
        out
    }

    fn resolve_path(&self, model: &str) -> Option<PathBuf> {
        self.list_models()
            .into_iter()
            .find(|(stem, _)| stem == model)
            .map(|(_, p)| p)
    }

    fn load_or_get(&self, model: &str, path: &Path) -> Result<Arc<WhisperContext>> {
        let mut guard = self.cached.lock();
        if let Some(c) = &*guard {
            if c.id == model {
                debug!(
                    target: TRACE_TARGET,
                    op = "load",
                    model,
                    cache = "hit",
                    "reusing cached model"
                );
                return Ok(c.ctx.clone());
            }
        }
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            path = %path.display(),
            "loading model"
        );
        let started = Instant::now();
        let params = WhisperContextParameters::default();
        let ctx = WhisperContext::new_with_params(
            path.to_str()
                .ok_or_else(|| anyhow!("model path not UTF-8: {}", path.display()))?,
            params,
        )
        .with_context(|| format!("loading whisper model from {}", path.display()))
        .inspect_err(|e| {
            warn!(
                target: TRACE_TARGET,
                op = "load",
                model,
                path = %path.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "failed to load model"
            );
        })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let arc = Arc::new(ctx);
        *guard = Some(CachedModel {
            id: model.to_string(),
            ctx: arc.clone(),
        });
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            elapsed_ms,
            "model loaded"
        );
        Ok(arc)
    }
}

/// Fetch the audio at `url` and decode it into a vector of mono f32
/// samples at 16 kHz (Whisper's expected input).  Supports WAV in 16-bit
/// PCM or 32-bit float, mono or stereo (downmixed by averaging).
pub fn fetch_and_decode_audio(url: &str) -> Result<Vec<f32>> {
    let bytes = if let Some(rest) = url.strip_prefix("file://") {
        let mut f =
            std::fs::File::open(rest).with_context(|| format!("opening local audio {rest}"))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        buf
    } else {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let response = client.get(url).send()?.error_for_status()?;
        response.bytes()?.to_vec()
    };
    decode_wav_to_mono_16khz(&bytes)
}

pub fn decode_wav_to_mono_16khz(bytes: &[u8]) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))
        .context("WAV parse: not a valid WAV file")?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let source_rate = spec.sample_rate;

    // Read into f32 samples in [-1, 1].
    let mut interleaved: Vec<f32> = Vec::new();
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => {
            for s in reader.samples::<i16>() {
                interleaved.push(s? as f32 / i16::MAX as f32);
            }
        }
        (hound::SampleFormat::Int, 32) => {
            for s in reader.samples::<i32>() {
                interleaved.push(s? as f32 / i32::MAX as f32);
            }
        }
        (hound::SampleFormat::Float, 32) => {
            for s in reader.samples::<f32>() {
                interleaved.push(s?);
            }
        }
        other => bail!(
            "unsupported WAV sample format: {:?} {} bits",
            other.0,
            other.1
        ),
    };

    // Downmix to mono.
    let mono: Vec<f32> = if channels == 1 {
        interleaved
    } else {
        let frames = interleaved.len() / channels;
        let mut mono = Vec::with_capacity(frames);
        for i in 0..frames {
            let mut sum = 0.0_f32;
            for c in 0..channels {
                sum += interleaved[i * channels + c];
            }
            mono.push(sum / channels as f32);
        }
        mono
    };

    // Resample to 16 kHz using linear interpolation.  Cheap, accurate
    // enough for Whisper at low-res model sizes; for >= small models
    // operators should provide 16 kHz audio directly.
    Ok(resample_linear(&mono, source_rate, 16_000))
}

fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == target_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = source_rate as f64 / target_rate as f64;
    let out_len = ((samples.len() as f64) / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let pos = n as f64 * ratio;
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        let a = samples[i.min(samples.len() - 1)];
        let b = samples[(i + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

impl Engine for WhisperEngine {
    fn name(&self) -> &'static str {
        "whisper"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let models: Vec<String> = self.list_models().into_iter().map(|(s, _)| s).collect();
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::AudioStt, models);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let stt = match task {
            Task::AudioStt(p) => p,
            other => {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    kind = kind.as_str(),
                    model,
                    "unsupported task kind"
                );
                return Err(crate::engine::UnsupportedTask::new("whisper", other.kind()).into());
            }
        };
        let path = self.resolve_path(model).ok_or_else(|| {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                model,
                models_root = %self.stt_dir().display(),
                "model not found"
            );
            anyhow!("model `{model}` not found in {}", self.stt_dir().display())
        })?;
        let ctx = self.load_or_get(model, &path)?;
        let fetch_started = Instant::now();
        let audio = fetch_and_decode_audio(&stt.input_url)
            .with_context(|| format!("decoding audio from {}", stt.input_url))
            .inspect_err(|e| {
                warn!(
                    target: TRACE_TARGET,
                    op = "fetch_audio",
                    url = %stt.input_url,
                    elapsed_ms = fetch_started.elapsed().as_millis() as u64,
                    error = %e,
                    "audio fetch/decode failed"
                );
            })?;
        let fetch_ms = fetch_started.elapsed().as_millis() as u64;
        if audio.is_empty() {
            warn!(
                target: TRACE_TARGET,
                op = "fetch_audio",
                url = %stt.input_url,
                "decoded audio is empty"
            );
            bail!("decoded audio is empty");
        }
        debug!(
            target: TRACE_TARGET,
            op = "fetch_audio",
            url = %stt.input_url,
            samples = audio.len(),
            duration_s = audio.len() as f32 / 16_000.0,
            elapsed_ms = fetch_ms,
            "audio ready"
        );

        let decode_started = Instant::now();
        let mut state = ctx.create_state().context("creating whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(num_cpus_for_decode() as i32);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if let Some(lang) = stt.language.as_deref() {
            params.set_language(Some(lang));
        }

        state.full(params, &audio).context("whisper full decode")?;
        let segments = state.full_n_segments().unwrap_or(0);
        let mut text = String::new();
        for i in 0..segments {
            if let Ok(s) = state.full_get_segment_text(i) {
                text.push_str(&s);
            }
        }
        let duration_seconds = audio.len() as f32 / 16_000.0;
        let decode_ms = decode_started.elapsed().as_millis() as u64;
        info!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = kind.as_str(),
            model,
            segments,
            audio_seconds = duration_seconds,
            decode_ms,
            "transcription complete"
        );
        let json = serde_json::json!({
            "text": text.trim(),
            "language": stt.language.unwrap_or_else(|| "en".into()),
            "duration": duration_seconds,
            "segments": segments,
        });
        Ok(TaskResult::AudioStt { json })
    }
}

fn num_cpus_for_decode() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_picks_up_bin_files() {
        let tmp = tempfile::tempdir().unwrap();
        let stt_dir = tmp.path().join("stt");
        std::fs::create_dir_all(&stt_dir).unwrap();
        std::fs::write(stt_dir.join("whisper-tiny.en.bin"), b"not-real").unwrap();
        std::fs::write(stt_dir.join("ignored.txt"), b"x").unwrap();
        let engine = WhisperEngine::new(tmp.path().to_path_buf());
        let caps = engine.capabilities();
        assert_eq!(
            caps.supported_models_per_kind[&TaskKind::AudioStt],
            vec!["whisper-tiny.en".to_string()]
        );
    }

    #[test]
    fn capabilities_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = WhisperEngine::new(tmp.path().to_path_buf());
        let caps = engine.capabilities();
        assert!(caps.supported_models_per_kind[&TaskKind::AudioStt].is_empty());
    }

    #[test]
    fn dispatch_rejects_non_stt_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = WhisperEngine::new(tmp.path().to_path_buf());
        let task = Task::AudioTts(AudioTtsParams {
            text: "x".into(),
            voice: "v".into(),
            ext: "wav".into(),
            ..Default::default()
        });
        let err = engine.dispatch("anything", task).unwrap_err();
        assert!(err.to_string().contains("cannot serve audio_tts"));
    }

    #[test]
    fn dispatch_errors_when_model_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = WhisperEngine::new(tmp.path().to_path_buf());
        let task = Task::AudioStt(AudioSttParams {
            input_url: "file:///dev/null".into(),
            language: None,
            ..Default::default()
        });
        let err = engine.dispatch("no-such", task).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resample_linear_passes_through_when_rates_match() {
        let samples = vec![0.1, 0.2, 0.3];
        let out = resample_linear(&samples, 16_000, 16_000);
        assert_eq!(out, samples);
    }

    #[test]
    fn resample_linear_downsamples_to_target_length() {
        let samples: Vec<f32> = (0..3200).map(|i| (i as f32) / 3200.0).collect();
        let out = resample_linear(&samples, 16_000, 8_000);
        assert_eq!(out.len(), 1600);
    }

    #[test]
    fn decode_wav_round_trips_mono_16bit_to_f32() {
        // Synthesise 0.1 s of sine wave at 16 kHz, encode WAV, decode.
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            for i in 0..1600 {
                let t = i as f32 / 16_000.0;
                let s = (t * 2.0 * std::f32::consts::PI * 440.0).sin();
                writer
                    .write_sample((s * 0.4 * i16::MAX as f32) as i16)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }
        let bytes = buf.into_inner();
        let samples = decode_wav_to_mono_16khz(&bytes).unwrap();
        assert_eq!(samples.len(), 1600);
        assert!(samples.iter().any(|s| s.abs() > 0.1));
    }

    #[test]
    fn fetch_and_decode_audio_supports_file_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("clip.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..1600 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let url = format!("file://{}", path.to_string_lossy());
        let samples = fetch_and_decode_audio(&url).unwrap();
        assert_eq!(samples.len(), 1600);
    }
}
