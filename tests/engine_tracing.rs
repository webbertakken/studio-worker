//! Proves every engine emits operator-visible `tracing` events on
//! dispatch and on its key failure paths.  Without these the only
//! breadcrumbs operators get from `RUST_LOG=studio_worker=debug` come
//! from `runtime::run_job`, which logs *that* a job took N seconds but
//! never which engine handled it, which model was loaded, or which
//! HTTP call to gradio actually went out.
//!
//! Uses the shared `studio_worker::test_support::capture` helper (aliased
//! locally as `captured_logs_for` to keep call sites readable).

use studio_worker::config::Config;
use studio_worker::engine::{self, render_procedural, Engine, SyntheticEngine};
use studio_worker::test_support::capture as captured_logs_for;
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn image_task(prompt: &str) -> Task {
    Task::Image(ImageParams {
        prompt: prompt.into(),
        width: 64,
        height: 64,
        steps: 1,
        seed: None,
        ext: "webp".into(),
    })
}

fn llm_task() -> Task {
    Task::Llm(LlmParams {
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
        max_tokens: 1,
        temperature: 0.0,
    })
}

// ---------------------------------------------------------------------------
// SyntheticEngine
// ---------------------------------------------------------------------------

#[test]
fn synthetic_engine_dispatch_emits_debug_event() {
    let logs = captured_logs_for(|| {
        let engine = SyntheticEngine::new(vec![]);
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
// GradioEngine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gradio_engine_dispatch_success_emits_debug_event() {
    use base64::Engine as _;
    let server = MockServer::start().await;
    let bytes = render_procedural("dragon", "png").expect("render");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let data_url = format!("data:image/png;base64,{b64}");
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [data_url] })),
        )
        .mount(&server)
        .await;

    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let cfg = Config {
            engine: "gradio".into(),
            gradio_endpoint_url: Some(uri),
            supported_models_override: vec!["tiny-test".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
        engine
            .dispatch("tiny-test", image_task("dragon"))
            .expect("dispatch");
    });
    assert!(
        logs.contains("studio_worker::engine::gradio"),
        "expected gradio target, got: {logs}"
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
        logs.contains("model=\"tiny-test\""),
        "expected model field: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field: {logs}"
    );
    assert!(
        logs.contains("/run/predict"),
        "expected predict URL in inner http log: {logs}"
    );
}

#[test]
fn gradio_engine_unsupported_kind_emits_warn() {
    let logs = captured_logs_for(|| {
        let cfg = Config {
            engine: "gradio".into(),
            gradio_endpoint_url: Some("http://example.invalid".into()),
            supported_models_override: vec!["tiny-test".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
        let _ = engine.dispatch("tiny-test", llm_task());
    });
    assert!(
        logs.contains("studio_worker::engine::gradio"),
        "expected gradio target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(logs.contains("kind=\"llm\""), "expected kind field: {logs}");
}

#[tokio::test]
async fn gradio_engine_5xx_emits_warn_with_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;
    let uri = server.uri();
    let logs = captured_logs_for(move || {
        let cfg = Config {
            engine: "gradio".into(),
            gradio_endpoint_url: Some(uri),
            supported_models_override: vec!["tiny-test".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
        let _ = engine.dispatch("tiny-test", image_task("anything"));
    });
    assert!(
        logs.contains("studio_worker::engine::gradio"),
        "expected gradio target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(logs.contains("status=503"), "expected status field: {logs}");
}

// ---------------------------------------------------------------------------
// MultiEngine
// ---------------------------------------------------------------------------

#[test]
fn multi_engine_pick_emits_debug_event_with_sub_engine() {
    let logs = captured_logs_for(|| {
        let cfg = Config {
            engine: "multi".into(),
            engines: vec!["synthetic".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
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
fn multi_engine_fallback_emits_debug_event() {
    // Synthetic only declares specific model ids — request an unknown
    // model id but a kind it advertises so the fallback branch fires.
    let logs = captured_logs_for(|| {
        let cfg = Config {
            engine: "multi".into(),
            engines: vec!["synthetic".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
        let _ = engine.dispatch("not-a-real-model", image_task("hi"));
    });
    assert!(
        logs.contains("match=\"fallback\""),
        "expected match=fallback: {logs}"
    );
}

#[test]
fn multi_engine_no_engine_for_kind_emits_warn() {
    // Build a synthetic-only multi engine, but ask for a kind it
    // doesn't advertise — actually synthetic advertises every kind, so
    // we need an empty multi engine.
    let logs = captured_logs_for(|| {
        let cfg = Config {
            engine: "multi".into(),
            // Even with synthetic engines configured, asking for a kind
            // it advertises succeeds; instead build an empty list via
            // build_multi error path — that won't trigger pick.  Use a
            // gradio-only multi (image-only) and request llm.
            engines: vec!["gradio".into()],
            gradio_endpoint_url: Some("http://example.invalid".into()),
            supported_models_override: vec!["tiny".into()],
            ..Config::default()
        };
        let engine = engine::build(&cfg).expect("build engine");
        let _ = engine.dispatch("tiny", llm_task());
    });
    assert!(
        logs.contains("studio_worker::engine::multi"),
        "expected multi target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(logs.contains("kind=\"llm\""), "expected kind field: {logs}");
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
            }),
        );
    });
    assert!(
        logs.contains("studio_worker::engine::candle_image"),
        "expected candle_image target, got: {logs}"
    );
    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
}
