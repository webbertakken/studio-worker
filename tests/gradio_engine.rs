//! Integration test for the `gradio` engine.
//!
//! Spins up a wiremock-based fake Gradio (no real GPU, no real model)
//! that returns a deterministic base64 image when the worker POSTs to
//! `/run/predict`.  Proves the GradioEngine extracts the bytes and hands
//! them back to the run loop.
//!
//! This is the cheap-models story the operator asked for: the gradio
//! engine code path is fully exercised in CI without touching VRAM.

use base64::Engine as _;
use studio_worker::config::Config;
use studio_worker::engine::{self, render_procedural};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cheap_payload(prompt: &str) -> Vec<u8> {
    // Reuse the synthetic renderer so the mock returns a valid PNG every time.
    render_procedural(prompt, "png").expect("render")
}

#[tokio::test]
async fn gradio_engine_decodes_base64_image() {
    let server = MockServer::start().await;
    let prompt = "stone golem";
    let bytes = cheap_payload(prompt);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let data_url = format!("data:image/png;base64,{b64}");

    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [data_url] })),
        )
        .mount(&server)
        .await;

    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };

    let engine = engine::build(&cfg).expect("build engine");
    assert_eq!(engine.supported_models(), vec!["tiny-test".to_string()]);

    let prompt_owned = prompt.to_string();
    let result = std::thread::spawn(move || engine.generate(&prompt_owned, "tiny-test", "webp"))
        .join()
        .expect("worker thread panicked")
        .expect("generate ok");
    assert_eq!(result, bytes, "round-trip should preserve image bytes");
}

#[tokio::test]
async fn gradio_engine_follows_image_url() {
    // Many Gradio apps return a URL pointing back at the same server.
    // The engine must follow that URL and fetch the bytes.
    let server = MockServer::start().await;
    let bytes = cheap_payload("phoenix");

    Mock::given(method("GET"))
        .and(path("/file/result.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;

    let url = format!("{}/file/result.png", server.uri());
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [url] })),
        )
        .mount(&server)
        .await;

    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };

    let engine = engine::build(&cfg).expect("build engine");
    let result = std::thread::spawn(move || engine.generate("phoenix", "tiny-test", "webp"))
        .join()
        .expect("worker thread panicked")
        .expect("generate ok");
    assert_eq!(result, bytes);
}

#[tokio::test]
async fn gradio_engine_errors_on_unsupported_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [42] })))
        .mount(&server)
        .await;

    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let result = std::thread::spawn(move || engine.generate("anything", "tiny-test", "webp"))
        .join()
        .expect("worker thread panicked");
    assert!(result.is_err());
}
