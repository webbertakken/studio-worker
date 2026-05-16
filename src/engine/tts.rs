//! Pure-Rust formant-based text-to-speech engine.
//!
//! Compiled in with `--features tts`.  Produces a real, decodable WAV
//! whose pitch contour follows the input text: each character is mapped
//! to a formant frequency, and a brief envelope-shaped sine wave is
//! emitted per character.  The result is robotic (no neural TTS) but
//! it's genuinely synthesized speech-adjacent audio that's
//! deterministic, intelligibility-poor, and completely self-contained —
//! no model files, no FFI, no install step.
//!
//! Operators who want natural-sounding TTS should wire up Piper (via
//! `piper-rs` once mature) or a cloud TTS in a follow-up iteration.
//! The trait + capability surface is ready for them.
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{bail, Result};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::collections::BTreeMap;
use std::io::Cursor;

pub struct TtsEngine;

impl TtsEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TtsEngine {
    fn default() -> Self {
        Self::new()
    }
}

const MODEL_ID: &str = "formant-synth";
const SAMPLE_RATE: u32 = 16_000;

/// Map a single character to a (formant_hz, duration_ms) tuple.  Vowels
/// get vocal-tract formants in the 250-800 Hz range; consonants get
/// shorter higher-frequency clicks; whitespace and punctuation get
/// silence with varying lengths.
fn formant_for(c: char) -> (f32, u32) {
    match c.to_ascii_lowercase() {
        'a' => (730.0, 95),
        'e' => (530.0, 80),
        'i' => (270.0, 75),
        'o' => (570.0, 95),
        'u' => (440.0, 100),
        'y' => (380.0, 90),
        'b' => (180.0, 50),
        'p' => (200.0, 45),
        'd' => (220.0, 50),
        't' => (250.0, 45),
        'g' => (190.0, 55),
        'k' => (260.0, 50),
        'f' => (340.0, 70),
        'v' => (300.0, 60),
        's' => (380.0, 65),
        'z' => (340.0, 60),
        'm' => (260.0, 60),
        'n' => (280.0, 55),
        'l' => (310.0, 55),
        'r' => (350.0, 60),
        'h' => (200.0, 35),
        'w' => (400.0, 75),
        'c' => (310.0, 50),
        'j' => (290.0, 50),
        'q' => (260.0, 55),
        'x' => (350.0, 55),
        ' ' | '\t' => (0.0, 80),   // silence
        '\n' | '\r' => (0.0, 120), // longer silence
        '.' | '?' | '!' => (0.0, 180),
        ',' | ';' | ':' => (0.0, 100),
        c if c.is_ascii_digit() => {
            // 220 Hz (0) ... 770 Hz (9) — covers a roughly octave-range
            let n = c.to_digit(10).unwrap_or(0) as f32;
            (220.0 + n * 60.0, 80)
        }
        _ => (440.0, 60), // unknown char: neutral A4
    }
}

/// Render a real 16-bit PCM WAV at 16 kHz mono from `text`.
pub fn render(text: &str, _voice: &str) -> Result<Vec<u8>> {
    let mut samples: Vec<f32> = Vec::new();
    for c in text.chars() {
        let (hz, ms) = formant_for(c);
        let n = (SAMPLE_RATE as u64 * u64::from(ms) / 1_000) as usize;
        let attack = (n / 6).max(1);
        let release = (n / 4).max(1);
        for i in 0..n {
            let t = i as f32 / SAMPLE_RATE as f32;
            let amplitude = if hz == 0.0 {
                0.0
            } else {
                // Linear attack + release envelope.
                let env_in = if i < attack {
                    i as f32 / attack as f32
                } else if i >= n - release {
                    (n - i) as f32 / release as f32
                } else {
                    1.0
                };
                env_in * 0.35
            };
            let s = if hz > 0.0 {
                amplitude * (2.0 * std::f32::consts::PI * hz * t).sin()
            } else {
                0.0
            };
            samples.push(s);
        }
        // Small inter-character pause to give the audio more rhythm.
        samples.resize(samples.len() + (SAMPLE_RATE / 1000 * 15) as usize, 0.0);
    }
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::new(&mut buf, spec)?;
        for s in &samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer.write_sample(v)?;
        }
        writer.finalize()?;
    }
    Ok(buf.into_inner())
}

impl Engine for TtsEngine {
    fn name(&self) -> &'static str {
        "tts"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::AudioTts, vec![MODEL_ID.to_string()]);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, _model: &str, task: Task) -> Result<TaskResult> {
        let params = match task {
            Task::AudioTts(p) => p,
            other => bail!("tts engine cannot serve {} tasks", other.kind().as_str()),
        };
        let bytes = render(&params.text, &params.voice)?;
        Ok(TaskResult::AudioTts {
            bytes,
            ext: "wav".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_audio_tts_kind() {
        let engine = TtsEngine::new();
        let caps = engine.capabilities();
        assert_eq!(
            caps.supported_models_per_kind[&TaskKind::AudioTts],
            vec![MODEL_ID.to_string()]
        );
        assert_eq!(engine.name(), "tts");
    }

    #[test]
    fn engine_default_constructs() {
        let _ = TtsEngine;
    }

    #[test]
    fn dispatch_rejects_non_tts_tasks() {
        let engine = TtsEngine::new();
        let err = engine
            .dispatch(
                MODEL_ID,
                Task::Image(ImageParams {
                    prompt: "x".into(),
                    width: 64,
                    height: 64,
                    steps: 1,
                    seed: None,
                    ext: "webp".into(),
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot serve image"));
    }

    #[test]
    fn render_produces_decodable_wav_with_correct_duration() {
        let bytes = render("hello", "default").unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, SAMPLE_RATE);
        assert_eq!(spec.channels, 1);
        // "hello" = h(35ms) + e(80ms) + l(55ms) + l(55ms) + o(95ms) +
        //   5 inter-char pauses (15ms each) = 395 ms.
        let duration_s = reader.duration() as f32 / spec.sample_rate as f32;
        assert!((0.35..0.5).contains(&duration_s), "got {duration_s}");
    }

    #[test]
    fn different_texts_produce_different_audio() {
        let a = render("hello", "default").unwrap();
        let b = render("world", "default").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn same_text_is_deterministic() {
        let a = render("studio", "default").unwrap();
        let b = render("studio", "default").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn render_handles_punctuation_and_digits() {
        let bytes = render("hello, world! 42.", "default").unwrap();
        let reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        assert!(reader.duration() > 0);
    }

    #[test]
    fn formant_for_returns_silence_on_whitespace() {
        assert_eq!(formant_for(' '), (0.0, 80));
        assert_eq!(formant_for('\n'), (0.0, 120));
        assert_eq!(formant_for('.'), (0.0, 180));
    }

    #[test]
    fn formant_for_maps_digits_to_octave_range() {
        let (hz0, _) = formant_for('0');
        let (hz9, _) = formant_for('9');
        assert!(hz0 < hz9);
    }

    #[test]
    fn formant_for_falls_back_to_neutral_on_unknown() {
        let (hz, _) = formant_for('@');
        assert_eq!(hz, 440.0);
    }
}
