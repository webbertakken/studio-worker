//! Proves every engine emits operator-visible `tracing` events on
//! dispatch and on its key failure paths.  Without these the only
//! breadcrumbs operators get from `RUST_LOG=studio_worker=debug` come
//! from the WS session, which logs *that* a job took N seconds but
//! never which engine handled it, which model was loaded, or which
//! sub-engine MultiEngine picked.
//!
//! Uses the shared `studio_worker::test_support::capture` helper (aliased
//! locally as `captured_logs_for` to keep call sites readable).

use studio_worker::config::Config;
use studio_worker::engine::{self, Engine, SyntheticEngine};
use studio_worker::test_support::capture as captured_logs_for;
use studio_worker::types::*;

fn image_task(prompt: &str) -> Task {
    Task::Image(ImageParams {
        prompt: prompt.into(),
        width: 64,
        height: 64,
        steps: 1,
        ext: "webp".into(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// SyntheticEngine
// ---------------------------------------------------------------------------

#[test]
fn synthetic_engine_dispatch_emits_debug_event() {
    let logs = captured_logs_for(|| {
        let engine = SyntheticEngine::new();
        engine.dispatch("synthetic", image_task("dragon")).unwrap();
    });
    assert!(
        logs.contains("studio_worker::engine::synthetic"),
        "expected synthetic target, got: {logs}"
    );
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"dispatch\""),
        "expected op field: {logs}"
    );
    assert!(
        logs.contains("kind=\"image\""),
        "expected kind field: {logs}"
    );
    assert!(
        logs.contains("model=\"synthetic\""),
        "expected model field: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field: {logs}"
    );
}

// ---------------------------------------------------------------------------
// MultiEngine — engine::build always wraps in MultiEngine now, so we
// can drive the routing trace events straight through it.
// ---------------------------------------------------------------------------

#[test]
fn multi_engine_pick_emits_debug_event_with_sub_engine() {
    let logs = captured_logs_for(|| {
        let engine = engine::build(&Config::default()).expect("build engine");
        engine.dispatch("synthetic", image_task("hi")).unwrap();
    });
    assert!(
        logs.contains("studio_worker::engine::multi"),
        "expected multi target, got: {logs}"
    );
    assert!(logs.contains("DEBUG"), "expected DEBUG event, got: {logs}");
    assert!(
        logs.contains("op=\"pick\""),
        "expected op=pick field: {logs}"
    );
    assert!(
        logs.contains("sub_engine=\"synthetic\""),
        "expected sub_engine field: {logs}"
    );
    assert!(
        logs.contains("match=\"exact\""),
        "expected match=exact: {logs}"
    );
}

#[test]
fn multi_engine_logs_no_engine_for_unknown_model() {
    // No fallback: an unknown model id (kind advertised by synthetic
    // but model never registered) is rejected.  We assert on the
    // "no engine claims this exact (kind, model) pair" warn line.
    let logs = captured_logs_for(|| {
        let engine = engine::build(&Config::default()).expect("build engine");
        let err = engine.dispatch("not-a-real-model", image_task("hi"));
        assert!(err.is_err(), "expected dispatch to reject unknown model");
    });
    assert!(
        logs.contains("no engine claims this exact"),
        "expected no-engine warn line: {logs}"
    );
}

// ---------------------------------------------------------------------------
// SdCppEngine — the always-on default image engine (no feature flag).
// Source-driven, so the rejection path runs through
// `dispatch_with_source`.  It short-circuits before resolving sd-cli or
// touching any file, so it's CI-safe.
// ---------------------------------------------------------------------------

#[test]
fn sdcpp_engine_dispatch_unsupported_kind_emits_warn() {
    use studio_worker::engine::sdcpp::SdCppEngine;
    let tmp = tempfile::tempdir().unwrap();
    let logs = captured_logs_for(move || {
        let engine = SdCppEngine::new(tmp.path());
        let _ = engine.dispatch_with_source(
            "sd-cpp:any",
            Task::AudioTts(AudioTtsParams {
                text: "x".into(),
                voice: "v".into(),
                ext: "wav".into(),
                ..Default::default()
            }),
            &ModelSource {
                engine: ModelEngine::SdCpp,
                files: vec![],
                cli_defaults: ModelCliDefaults::default(),
            },
        );
    });
    assert!(
        logs.contains("studio_worker::engine::sdcpp"),
        "expected sdcpp target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"dispatch\""),
        "expected op=dispatch field: {logs}"
    );
    assert!(
        logs.contains("kind=\"audio_tts\""),
        "expected kind field: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Feature-gated engines — cheap dispatch-rejection paths only.  The
// heavy "loading model" tracing is exercised by `tests/real_*.rs`,
// which run only when `RUN_REAL_ENGINE_TESTS=1`.
// ---------------------------------------------------------------------------

#[cfg(feature = "tts")]
#[test]
fn tts_engine_dispatch_emits_trace() {
    use studio_worker::engine::tts::TtsEngine;
    let logs = captured_logs_for(|| {
        let engine = TtsEngine::new();
        engine
            .dispatch(
                "formant-synth",
                Task::AudioTts(AudioTtsParams {
                    text: "hi".into(),
                    voice: "default".into(),
                    ext: "wav".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
    });
    assert!(
        logs.contains("studio_worker::engine::tts"),
        "expected tts target, got: {logs}"
    );
    assert!(
        logs.contains("op=\"dispatch\""),
        "expected op field: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field: {logs}"
    );
}

#[cfg(feature = "video")]
#[test]
fn video_engine_dispatch_emits_trace() {
    use studio_worker::engine::video::VideoEngine;
    let logs = captured_logs_for(|| {
        let engine = VideoEngine::new();
        engine
            .dispatch(
                "procedural-gif",
                Task::Video(VideoParams {
                    prompt: "x".into(),
                    seconds: 0.2,
                    width: 64,
                    height: 64,
                    ext: "gif".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
    });
    assert!(
        logs.contains("studio_worker::engine::video"),
        "expected video target, got: {logs}"
    );
    assert!(
        logs.contains("op=\"dispatch\""),
        "expected op field: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field: {logs}"
    );
}

#[cfg(feature = "llama")]
#[test]
fn llama_engine_dispatch_unsupported_kind_emits_warn() {
    use studio_worker::engine::llama::LlamaEngine;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_path_buf();
    let logs = captured_logs_for(move || {
        let engine = LlamaEngine::new(path).expect("init backend");
        let _ = engine.dispatch(
            "anything",
            Task::Image(ImageParams {
                prompt: "x".into(),
                width: 64,
                height: 64,
                steps: 1,
                seed: None,
                ext: "webp".into(),
                ..Default::default()
            }),
        );
    });
    assert!(
        logs.contains("studio_worker::engine::llama"),
        "expected llama target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
}

#[cfg(feature = "whisper")]
#[test]
fn whisper_engine_dispatch_unsupported_kind_emits_warn() {
    use studio_worker::engine::whisper::WhisperEngine;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_path_buf();
    let logs = captured_logs_for(move || {
        let engine = WhisperEngine::new(path);
        let _ = engine.dispatch(
            "anything",
            Task::AudioTts(AudioTtsParams {
                text: "x".into(),
                voice: "v".into(),
                ext: "wav".into(),
                ..Default::default()
            }),
        );
    });
    assert!(
        logs.contains("studio_worker::engine::whisper"),
        "expected whisper target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
}

#[cfg(feature = "image-candle")]
#[test]
fn candle_image_engine_dispatch_unsupported_kind_emits_warn() {
    use studio_worker::engine::candle_image::CandleImageEngine;
    let logs = captured_logs_for(|| {
        let engine = CandleImageEngine::new();
        let _ = engine.dispatch(
            "stable-diffusion-v1-5",
            Task::Llm(LlmParams {
                messages: vec![],
                max_tokens: 1,
                temperature: 0.0,
                ..Default::default()
            }),
        );
    });
    assert!(
        logs.contains("studio_worker::engine::candle_image"),
        "expected candle_image target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
}

// ---------------------------------------------------------------------------
// OnnxImageEngine (LaMa object-removal).  Source-driven, so we drive it
// through `dispatch_with_source`.  Both paths short-circuit before any
// network / onnxruntime native call, so they're CI-safe.
// ---------------------------------------------------------------------------

#[cfg(feature = "image-onnx")]
fn onnx_source(files: Vec<ModelFile>) -> ModelSource {
    ModelSource {
        engine: ModelEngine::Onnx,
        files,
        cli_defaults: ModelCliDefaults::default(),
    }
}

#[cfg(feature = "image-onnx")]
#[test]
fn onnx_engine_dispatch_unsupported_kind_emits_warn() {
    use studio_worker::engine::onnx::OnnxImageEngine;
    let tmp = tempfile::tempdir().unwrap();
    let logs = captured_logs_for(move || {
        let engine = OnnxImageEngine::new(tmp.path().to_path_buf());
        let _ = engine.dispatch_with_source(
            "lama",
            Task::AudioTts(AudioTtsParams {
                text: "x".into(),
                voice: "v".into(),
                ext: "wav".into(),
                ..Default::default()
            }),
            &onnx_source(vec![]),
        );
    });
    assert!(
        logs.contains("studio_worker::engine::onnx"),
        "expected onnx target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
}

#[cfg(feature = "image-onnx")]
#[test]
fn onnx_engine_removal_failure_emits_warn() {
    use studio_worker::engine::onnx::OnnxImageEngine;
    // A removal job with no `initImageUrl` fails before any download or
    // onnxruntime call, so this exercises the failure-logging path on a
    // GPU-less CI runner.  The engine must surface *why* the removal
    // failed under its own target, not stay silent and leave the only
    // breadcrumb to the WS session.
    let tmp = tempfile::tempdir().unwrap();
    let logs = captured_logs_for(move || {
        let engine = OnnxImageEngine::new(tmp.path().to_path_buf());
        let err = engine.dispatch_with_source("lama", image_task("x"), &onnx_source(vec![]));
        assert!(
            err.is_err(),
            "expected removal to fail without an initImageUrl"
        );
    });
    assert!(
        logs.contains("studio_worker::engine::onnx"),
        "expected onnx target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"dispatch\""),
        "expected op=dispatch field: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field: {logs}"
    );
}
