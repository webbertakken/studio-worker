//! Energy voice-activity detection: after speech, a long enough run of
//! silence ends the utterance (hands-free finalise).
//!
//! Same rule and numbers as the Python Parakeet server it replaces, so the
//! app's hands-free behaviour does not change: RMS over 200 ms windows,
//! 0.018 threshold, 1.5 s of post-speech silence.

/// PCM sample rate every streaming model takes.
pub const SAMPLE_RATE: usize = 16_000;
/// Window the RMS is measured over.
pub const VAD_WINDOW_MS: usize = 200;
/// RMS at or above which a window counts as speech (mic levels measured
/// on the phone by the Python server; safe range 0.005..=0.05).
pub const VAD_THRESHOLD: f32 = 0.018;
/// Post-speech silence that ends a server-endpointed utterance; safe range
/// 800..=3000.  Clients that need longer pauses ask for client endpointing.
pub const VAD_SILENCE_MS: usize = 1500;

const WINDOW: usize = SAMPLE_RATE * VAD_WINDOW_MS / 1000;

/// Tracks speech and the silence run after it.
#[derive(Debug)]
pub struct Vad {
    carry: Vec<f32>,
    speech_ms: usize,
    silence_ms: usize,
    ends_after_ms: usize,
}

impl Default for Vad {
    fn default() -> Self {
        Self::with_silence_ms(VAD_SILENCE_MS)
    }
}

impl Vad {
    /// A detector that ends the utterance after `ends_after_ms` of
    /// post-speech silence.
    pub fn with_silence_ms(ends_after_ms: usize) -> Self {
        Self {
            carry: Vec::new(),
            speech_ms: 0,
            silence_ms: 0,
            ends_after_ms,
        }
    }

    /// Feed samples; true once speech was heard and silence has lasted
    /// long enough since.
    pub fn feed(&mut self, samples: &[f32]) -> bool {
        self.carry.extend_from_slice(samples);
        let whole = self.carry.len() / WINDOW * WINDOW;
        for window in self.carry[..whole].chunks(WINDOW) {
            let rms = (window.iter().map(|x| x * x).sum::<f32>() / WINDOW as f32).sqrt();
            if rms >= VAD_THRESHOLD {
                self.speech_ms += VAD_WINDOW_MS;
                self.silence_ms = 0;
            } else if self.speech_ms > 0 {
                self.silence_ms += VAD_WINDOW_MS;
            }
        }
        self.carry.drain(..whole);
        self.speech_ms > 0 && self.silence_ms >= self.ends_after_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amplitude: f32) -> Vec<f32> {
        (0..ms * 16)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn silence_alone_never_ends_an_utterance() {
        let mut vad = Vad::default();
        assert!(!vad.feed(&tone(5000, 0.0)));
    }

    #[test]
    fn speech_then_enough_silence_ends_it() {
        let mut vad = Vad::default();
        assert!(!vad.feed(&tone(600, 0.2)));
        assert!(!vad.feed(&tone(1400, 0.0)), "1.4 s is not yet enough");
        assert!(
            vad.feed(&tone(200, 0.0)),
            "1.6 s of silence after speech ends it"
        );
    }

    #[test]
    fn speech_resets_the_silence_run() {
        let mut vad = Vad::default();
        vad.feed(&tone(400, 0.2));
        vad.feed(&tone(1000, 0.0));
        vad.feed(&tone(200, 0.2));
        assert!(!vad.feed(&tone(1400, 0.0)));
        assert!(vad.feed(&tone(200, 0.0)));
    }

    #[test]
    fn windows_span_chunk_boundaries() {
        // 50 ms chunks: windows are only judged once 200 ms has arrived.
        let mut vad = Vad::default();
        for _ in 0..8 {
            vad.feed(&tone(50, 0.2));
        }
        let mut ended = false;
        for _ in 0..40 {
            ended |= vad.feed(&tone(50, 0.0));
        }
        assert!(ended);
    }

    #[test]
    fn a_longer_silence_threshold_waits_longer() {
        let mut vad = Vad::with_silence_ms(3000);
        vad.feed(&tone(400, 0.2));
        assert!(!vad.feed(&tone(2800, 0.0)), "2.8 s is under 3 s");
        assert!(vad.feed(&tone(200, 0.0)), "3 s of silence ends it");
    }

    #[test]
    fn quiet_noise_below_the_threshold_is_silence() {
        let mut vad = Vad::default();
        assert!(!vad.feed(&tone(2000, VAD_THRESHOLD * 0.5)));
    }
}
