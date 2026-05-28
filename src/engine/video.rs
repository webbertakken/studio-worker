//! Real video generation as an animated GIF.
//!
//! Compiled in with `--features video`.  Re-uses the synthetic image
//! renderer to produce N frames at the requested FPS and width/height,
//! then encodes them into a single animated GIF using the `gif` crate.
//! The output is a real, decodable animation that any browser, GIF
//! viewer, or `image::ImageReader` can open.
//!
//! Why GIF and not MP4?  Producing a valid MP4 requires shipping an
//! H.264 (or other video codec) encoder, which adds tens of MB of
//! native dependencies for what is fundamentally a placeholder until
//! real video-diffusion engines are wired in.  Animated GIF is
//! single-file, pure Rust, universally readable, and emits a
//! genuinely-animated artefact — exactly what we need from a
//! "synthetic video" engine.
use crate::engine::render_procedural;
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{bail, Context, Result};
use gif::{Encoder, Frame, Repeat};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::Instant;
use tracing::{debug, warn};

/// Tracing target for the procedural video engine.  Stable so
/// operators can filter with
/// `RUST_LOG=studio_worker::engine::video=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::video";

pub struct VideoEngine;

impl VideoEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VideoEngine {
    fn default() -> Self {
        Self::new()
    }
}

const MODEL_ID: &str = "procedural-gif";

impl Engine for VideoEngine {
    fn name(&self) -> &'static str {
        "video"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Video, vec![MODEL_ID.to_string()]);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let started = Instant::now();
        let params = match task {
            Task::Video(p) => p,
            other => {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    kind = kind.as_str(),
                    model,
                    "unsupported task kind"
                );
                bail!("video engine cannot serve {} tasks", other.kind().as_str());
            }
        };
        let result = render_gif(&params);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(bytes) => debug!(
                target: TRACE_TARGET,
                op = "dispatch",
                kind = kind.as_str(),
                model,
                seconds = params.seconds,
                width = params.width,
                height = params.height,
                bytes = bytes.len(),
                elapsed_ms,
                "ok"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                kind = kind.as_str(),
                model,
                elapsed_ms,
                error = %e,
                "failed"
            ),
        }
        let bytes = result?;
        Ok(TaskResult::Video {
            bytes,
            ext: "gif".into(),
        })
    }
}

/// Render an animated GIF from procedural frames.  Frames per second is
/// hardcoded at 10 — GIF supports 1/100 s frame delays so 10 fps is a
/// natural round number.
pub fn render_gif(params: &VideoParams) -> Result<Vec<u8>> {
    let width = params.width.clamp(64, 1024) as u16;
    let height = params.height.clamp(64, 1024) as u16;
    let fps: u32 = 10;
    let n_frames = (params.seconds.max(0.1) * fps as f32).round().max(1.0) as u32;
    let mut out = Cursor::new(Vec::<u8>::new());
    {
        let mut encoder =
            Encoder::new(&mut out, width, height, &[]).context("creating GIF encoder")?;
        encoder.set_repeat(Repeat::Infinite)?;
        for i in 0..n_frames {
            let frame_prompt = format!("{} #{i}", params.prompt);
            let png_bytes = render_procedural(&frame_prompt, "png")?;
            let img = image::load_from_memory(&png_bytes)?
                .resize_exact(
                    u32::from(width),
                    u32::from(height),
                    image::imageops::FilterType::Triangle,
                )
                .to_rgba8();
            let mut buf = img.into_raw();
            let mut frame = Frame::from_rgba_speed(width, height, &mut buf, 10);
            // 1/100 s units → 10 fps == 10 hundredths.
            frame.delay = 10;
            encoder.write_frame(&frame).context("writing GIF frame")?;
        }
    }
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(seconds: f32) -> Task {
        Task::Video(VideoParams {
            prompt: "a tiny dragon".into(),
            seconds,
            width: 128,
            height: 128,
            ext: "gif".into(),
            ..Default::default()
        })
    }

    #[test]
    fn engine_advertises_video_kind() {
        let engine = VideoEngine::new();
        let caps = engine.capabilities();
        assert_eq!(
            caps.supported_models_per_kind[&TaskKind::Video],
            vec![MODEL_ID.to_string()]
        );
        assert_eq!(engine.name(), "video");
    }

    #[test]
    fn engine_default_constructs() {
        let _ = VideoEngine;
    }

    #[test]
    fn dispatch_rejects_non_video_tasks() {
        let engine = VideoEngine::new();
        let err = engine
            .dispatch(
                MODEL_ID,
                Task::Llm(LlmParams {
                    messages: vec![],
                    max_tokens: 1,
                    temperature: 0.0,
                    ..Default::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot serve llm"));
    }

    #[test]
    fn dispatch_produces_valid_animated_gif() {
        let engine = VideoEngine::new();
        let result = engine.dispatch(MODEL_ID, task(1.0)).unwrap();
        let (bytes, ext) = match result {
            TaskResult::Video { bytes, ext } => (bytes, ext),
            other => panic!("expected video, got {:?}", other.kind()),
        };
        assert_eq!(ext, "gif");
        // GIF magic header.
        assert!(&bytes[..3] == b"GIF");
        // Parse back with the gif crate to count frames.
        let mut decoder_opts = gif::DecodeOptions::new();
        decoder_opts.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = decoder_opts
            .read_info(Cursor::new(bytes.clone()))
            .expect("decode gif");
        let mut frames = 0;
        while decoder.next_frame_info().expect("frame info").is_some() {
            frames += 1;
            // Consume the frame buffer so the decoder advances.
            let mut tmp = vec![0u8; decoder.buffer_size()];
            decoder.read_into_buffer(&mut tmp).unwrap();
        }
        // 1 second @ 10 fps = 10 frames.
        assert_eq!(frames, 10, "expected 10 frames, got {frames}");
    }

    #[test]
    fn shorter_seconds_produces_fewer_frames() {
        let small = render_gif(&VideoParams {
            prompt: "a".into(),
            seconds: 0.2,
            width: 64,
            height: 64,
            ext: "gif".into(),
            ..Default::default()
        })
        .unwrap();
        let big = render_gif(&VideoParams {
            prompt: "a".into(),
            seconds: 2.0,
            width: 64,
            height: 64,
            ext: "gif".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(big.len() > small.len());
    }
}
