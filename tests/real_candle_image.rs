//! Real image generation E2E via candle Stable Diffusion v1.5.
//!
//! SD weights are ~4 GB so we never download them automatically — the
//! test is skipped unless `RUN_REAL_CANDLE_TESTS=1` AND the
//! `stable-diffusion-v1-5` repo is already cached in the standard HF
//! cache (e.g. via the candle-examples crate, or a previous run of
//! this test with the env var set).
//!
//! On a modern CPU SD 1.5 at 256×256 with 4 steps takes several
//! minutes.  Run with `cargo test --release --features image-candle`
//! to keep it bearable.
#![cfg(feature = "image-candle")]

use std::io::Cursor;
use std::path::PathBuf;
use studio_worker::engine::candle_image::CandleImageEngine;
use studio_worker::engine::Engine;
use studio_worker::types::*;

fn hf_repo_present() -> bool {
    // candle-transformers and hf-hub use the standard HF cache layout:
    //   $HF_HOME/hub/models--<org>--<name>/...
    let home = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let candidates: [PathBuf; 2] = [
        home.join("huggingface")
            .join("hub")
            .join("models--stable-diffusion-v1-5--stable-diffusion-v1-5"),
        home.join("huggingface")
            .join("hub")
            .join("models--runwayml--stable-diffusion-v1-5"),
    ];
    candidates.iter().any(|p| p.exists())
}

#[test]
fn end_to_end_image_generation() {
    if std::env::var_os("RUN_REAL_CANDLE_TESTS").is_none() {
        eprintln!(
            "[real_candle_image] skipped: set RUN_REAL_CANDLE_TESTS=1 + ensure the \\\n  stable-diffusion-v1-5 weights are cached in HF_HOME (~4 GB)."
        );
        return;
    }
    if !hf_repo_present() {
        eprintln!(
            "[real_candle_image] skipped: SD 1.5 weights not pre-cached; \\\n  pre-download via candle-examples or hf-hub-cli, then re-run."
        );
        return;
    }

    let engine = CandleImageEngine::new();
    let caps = engine.capabilities();
    assert!(caps
        .supported_models_per_kind
        .get(&TaskKind::Image)
        .map(|m| m.contains(&"stable-diffusion-v1-5".into()))
        .unwrap_or(false));

    let task = Task::Image(ImageParams {
        prompt: "a cinematic stone golem on a misty mountain pass, fantasy concept art".into(),
        width: 256,
        height: 256,
        steps: 4,
        seed: Some(42),
        ext: "png".into(),
    });
    let started = std::time::Instant::now();
    let result = engine
        .dispatch("stable-diffusion-v1-5", task)
        .expect("inference should succeed");
    eprintln!("[real_candle_image] inference took {:?}", started.elapsed());
    let (bytes, ext) = match result {
        TaskResult::Image { bytes, ext } => (bytes, ext),
        other => panic!("expected image, got {:?}", other.kind()),
    };
    assert_eq!(ext, "png");
    assert!(bytes.len() > 1024, "PNG should be at least 1 KB");
    let img = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .expect("real PNG");
    assert_eq!(img.width(), 256);
    assert_eq!(img.height(), 256);
}
