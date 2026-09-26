//! One streaming speech-to-text session: the `/transcribe` protocol over
//! any streaming transcriber.  Pure (no sockets), so every rule is tested.
//!
//! Client: binary frames of 16 kHz mono s16le PCM; text `end` finalises;
//! text `cancel` closes without a final.  Server: `{"partial":true,"text"}`
//! as the transcript grows, `{"final":true,"text"}` once, `{"error"}`.

use super::vad::{Vad, SAMPLE_RATE};

/// Silence appended on finalise so the model emits its last words (a
/// streaming model lags its input by up to a chunk plus look-ahead).
/// Measured: 1.5 s flushed the last word in the spike.  Safe range 0.5..=3 s.
pub const FLUSH_SILENCE_MS: usize = 1500;

/// A streaming speech model with per-utterance state.
pub trait StreamingTranscriber {
    /// Samples the model takes per step (e.g. 2560 = 160 ms).
    fn chunk_samples(&self) -> usize;
    /// Feed one chunk; returns the text it adds (may be empty).
    fn step(&mut self, chunk: &[f32]) -> anyhow::Result<String>;
    /// Forget the previous utterance.
    fn reset(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    /// 16 kHz mono s16le PCM.
    Audio(Vec<u8>),
    End,
    Cancel,
}

impl ClientFrame {
    /// A text frame by name; `None` for anything unknown.
    pub fn from_text(text: &str) -> Option<Self> {
        match text.trim() {
            "end" => Some(Self::End),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    Partial(String),
    Final(String),
    Error(String),
}

impl ServerFrame {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Partial(text) => serde_json::json!({ "partial": true, "text": text }),
            Self::Final(text) => serde_json::json!({ "final": true, "text": text }),
            Self::Error(error) => serde_json::json!({ "error": error }),
        }
    }
}

/// Keep the socket open, or close it after sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Continue,
    Close,
}

/// SentencePiece word markers to spaces, whitespace collapsed.
pub fn normalise(raw: &str) -> String {
    raw.replace('\u{2581}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One utterance over a transcriber.
pub struct StreamSession<'a> {
    transcriber: &'a mut dyn StreamingTranscriber,
    pending: Vec<f32>,
    odd_byte: Option<u8>,
    raw: String,
    sent: String,
    vad: Vad,
}

impl<'a> StreamSession<'a> {
    pub fn new(transcriber: &'a mut dyn StreamingTranscriber) -> Self {
        transcriber.reset();
        Self {
            transcriber,
            pending: Vec::new(),
            odd_byte: None,
            raw: String::new(),
            sent: String::new(),
            vad: Vad::default(),
        }
    }

    /// Handle one client frame: the frames to send, then whether to close.
    pub fn handle(&mut self, frame: ClientFrame) -> (Vec<ServerFrame>, Next) {
        match frame {
            ClientFrame::Audio(bytes) => {
                let samples = self.decode(&bytes);
                let ended = self.vad.feed(&samples);
                self.pending.extend_from_slice(&samples);
                let mut out = Vec::new();
                if let Err(err) = self.drain(&mut out, true) {
                    out.push(failure(&err));
                    return (out, Next::Close);
                }
                if ended {
                    return self.finish(out);
                }
                (out, Next::Continue)
            }
            ClientFrame::End => self.finish(Vec::new()),
            ClientFrame::Cancel => (Vec::new(), Next::Close),
        }
    }

    fn decode(&mut self, bytes: &[u8]) -> Vec<f32> {
        let mut joined = Vec::with_capacity(bytes.len() + 1);
        joined.extend(self.odd_byte.take());
        joined.extend_from_slice(bytes);
        if joined.len() % 2 == 1 {
            self.odd_byte = joined.pop();
        }
        joined
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect()
    }

    /// Step every whole chunk; send a partial when the transcript changed.
    fn drain(&mut self, out: &mut Vec<ServerFrame>, partials: bool) -> anyhow::Result<()> {
        let chunk = self.transcriber.chunk_samples().max(1);
        while self.pending.len() >= chunk {
            let piece: Vec<f32> = self.pending.drain(..chunk).collect();
            self.raw.push_str(&self.transcriber.step(&piece)?);
            let text = normalise(&self.raw);
            if partials && text != self.sent {
                out.push(ServerFrame::Partial(text.clone()));
                self.sent = text;
            }
        }
        Ok(())
    }

    /// Flush the tail with silence, then send the final transcript.
    fn finish(&mut self, mut out: Vec<ServerFrame>) -> (Vec<ServerFrame>, Next) {
        let chunk = self.transcriber.chunk_samples().max(1);
        let flush = SAMPLE_RATE * FLUSH_SILENCE_MS / 1000;
        let target = (self.pending.len() + flush).div_ceil(chunk) * chunk;
        self.pending.resize(target, 0.0);
        match self.drain(&mut out, false) {
            Ok(()) => out.push(ServerFrame::Final(normalise(&self.raw))),
            Err(err) => out.push(failure(&err)),
        }
        (out, Next::Close)
    }
}

fn failure(err: &anyhow::Error) -> ServerFrame {
    ServerFrame::Error(format!("transcription failed: {err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emits one word per chunk from a script; counts resets.
    struct Scripted {
        words: Vec<&'static str>,
        next: usize,
        resets: usize,
        chunk: usize,
        fail_at: Option<usize>,
    }

    impl Scripted {
        fn new(words: &[&'static str]) -> Self {
            Self {
                words: words.to_vec(),
                next: 0,
                resets: 0,
                chunk: 1600,
                fail_at: None,
            }
        }
    }

    impl StreamingTranscriber for Scripted {
        fn chunk_samples(&self) -> usize {
            self.chunk
        }
        fn step(&mut self, _chunk: &[f32]) -> anyhow::Result<String> {
            if self.fail_at == Some(self.next) {
                anyhow::bail!("cuda went away");
            }
            let piece = self.words.get(self.next).copied().unwrap_or("");
            self.next += 1;
            Ok(piece.to_string())
        }
        fn reset(&mut self) {
            self.resets += 1;
            self.next = 0;
        }
    }

    /// `ms` of PCM at a constant level, as s16le bytes.
    fn pcm(ms: usize, level: i16) -> Vec<u8> {
        (0..ms * 16)
            .flat_map(|i| (if i % 2 == 0 { level } else { -level }).to_le_bytes())
            .collect()
    }

    fn texts(frames: &[ServerFrame]) -> Vec<String> {
        frames
            .iter()
            .map(|f| match f {
                ServerFrame::Partial(t) => format!("partial:{t}"),
                ServerFrame::Final(t) => format!("final:{t}"),
                ServerFrame::Error(e) => format!("error:{e}"),
            })
            .collect()
    }

    #[test]
    fn a_session_starts_from_a_clean_transcriber() {
        let mut t = Scripted::new(&[]);
        StreamSession::new(&mut t);
        assert_eq!(t.resets, 1);
    }

    #[test]
    fn partials_grow_as_chunks_arrive() {
        let mut t = Scripted::new(&["\u{2581}hello", "\u{2581}runa"]);
        let mut s = StreamSession::new(&mut t);
        let (out, next) = s.handle(ClientFrame::Audio(pcm(200, 8000)));
        assert_eq!(next, Next::Continue);
        assert_eq!(texts(&out), ["partial:hello", "partial:hello runa"]);
    }

    #[test]
    fn an_unchanged_transcript_sends_no_partial() {
        let mut t = Scripted::new(&["hi", "", ""]);
        let mut s = StreamSession::new(&mut t);
        let (out, _) = s.handle(ClientFrame::Audio(pcm(300, 8000)));
        assert_eq!(texts(&out), ["partial:hi"]);
    }

    #[test]
    fn audio_is_buffered_until_a_whole_chunk_arrives() {
        let mut t = Scripted::new(&["one"]);
        let mut s = StreamSession::new(&mut t);
        let (out, _) = s.handle(ClientFrame::Audio(pcm(50, 8000)));
        assert!(out.is_empty(), "50 ms is less than one 100 ms chunk");
        let (out, _) = s.handle(ClientFrame::Audio(pcm(50, 8000)));
        assert_eq!(texts(&out), ["partial:one"]);
    }

    #[test]
    fn an_odd_byte_carries_over_to_the_next_frame() {
        let mut t = Scripted::new(&["one"]);
        let mut s = StreamSession::new(&mut t);
        let bytes = pcm(100, 8000);
        let (a, b) = bytes.split_at(1601);
        assert!(s.handle(ClientFrame::Audio(a.to_vec())).0.is_empty());
        let (out, _) = s.handle(ClientFrame::Audio(b.to_vec()));
        assert_eq!(texts(&out), ["partial:one"]);
    }

    #[test]
    fn end_flushes_and_sends_the_final_then_closes() {
        let mut t = Scripted::new(&["hello", " there", " friend"]);
        let mut s = StreamSession::new(&mut t);
        s.handle(ClientFrame::Audio(pcm(100, 8000)));
        let (out, next) = s.handle(ClientFrame::End);
        assert_eq!(next, Next::Close);
        assert_eq!(texts(&out).last().unwrap(), "final:hello there friend");
    }

    #[test]
    fn cancel_closes_without_a_final() {
        let mut t = Scripted::new(&["x"]);
        let mut s = StreamSession::new(&mut t);
        let (out, next) = s.handle(ClientFrame::Cancel);
        assert_eq!(next, Next::Close);
        assert!(out.is_empty());
    }

    #[test]
    fn silence_after_speech_finalises_hands_free() {
        let mut t = Scripted::new(&["hi"]);
        let mut s = StreamSession::new(&mut t);
        s.handle(ClientFrame::Audio(pcm(400, 8000)));
        let (out, next) = s.handle(ClientFrame::Audio(pcm(1600, 0)));
        assert_eq!(next, Next::Close);
        assert_eq!(texts(&out).last().unwrap(), "final:hi");
    }

    #[test]
    fn a_transcriber_failure_is_reported_and_closes() {
        let mut t = Scripted::new(&["a", "b"]);
        t.fail_at = Some(1);
        let mut s = StreamSession::new(&mut t);
        let (out, next) = s.handle(ClientFrame::Audio(pcm(200, 8000)));
        assert_eq!(next, Next::Close);
        assert_eq!(
            texts(&out),
            ["partial:a", "error:transcription failed: cuda went away"]
        );
    }

    #[test]
    fn frames_serialise_to_the_wire_protocol() {
        assert_eq!(
            ServerFrame::Partial("hi".into()).to_json(),
            serde_json::json!({ "partial": true, "text": "hi" })
        );
        assert_eq!(
            ServerFrame::Final("hi".into()).to_json(),
            serde_json::json!({ "final": true, "text": "hi" })
        );
        assert_eq!(
            ServerFrame::Error("x".into()).to_json(),
            serde_json::json!({ "error": "x" })
        );
    }

    #[test]
    fn client_text_frames_parse_by_name() {
        assert_eq!(ClientFrame::from_text("end"), Some(ClientFrame::End));
        assert_eq!(
            ClientFrame::from_text(" cancel "),
            Some(ClientFrame::Cancel)
        );
        assert_eq!(ClientFrame::from_text("hello"), None);
    }

    #[test]
    fn pieces_normalise_to_plain_text() {
        assert_eq!(
            normalise("\u{2581}hello\u{2581}\u{2581}world  "),
            "hello world"
        );
        assert_eq!(normalise(" Hello Runa, please "), "Hello Runa, please");
    }
}
