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
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cheap_payload(prompt: &str) -> Vec<u8> {
    render_procedural(prompt, "png").expect("render")
}

fn image_task(prompt: &str) -> Task {
    Task::Image(ImageParams {
        prompt: prompt.into(),
        width: 512,
        height: 512,
        steps: 20,
        seed: None,
        ext: "webp".into(),
    })
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
    let caps = engine.capabilities();
    assert!(caps.supports(TaskKind::Image, "tiny-test"));

    let prompt_owned = prompt.to_string();
    let task = image_task(&prompt_owned);
    let result = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("worker thread panicked")
        .expect("dispatch ok");
    let (got, ext) = match result {
        TaskResult::Image { bytes, ext } => (bytes, ext),
        other => panic!("expected image, got {:?}", other.kind()),
    };
    assert_eq!(ext, "webp");
    assert_eq!(got, bytes, "round-trip should preserve image bytes");
}

#[tokio::test]
async fn gradio_engine_follows_image_url() {
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
    let task = image_task("phoenix");
    let result = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("worker thread panicked")
        .expect("dispatch ok");
    let got = match result {
        TaskResult::Image { bytes, .. } => bytes,
        other => panic!("expected image, got {:?}", other.kind()),
    };
    assert_eq!(got, bytes);
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
    let task = image_task("anything");
    let result = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("worker thread panicked");
    assert!(result.is_err());
}

#[tokio::test]
async fn gradio_engine_surfaces_5xx_from_predict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let task = image_task("upstream-down");
    let err = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("worker thread panicked")
        .unwrap_err();
    assert!(err.to_string().contains("gradio returned 503"));
}

#[tokio::test]
async fn gradio_engine_errors_when_response_missing_data() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let task = image_task("missing-data");
    let err = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("thread")
        .unwrap_err();
    assert!(err.to_string().contains("missing data[0]"));
}

#[tokio::test]
async fn gradio_engine_errors_when_image_url_returns_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "data": ["/file/missing.png"] })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/file/missing.png"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let task = image_task("fetch-fail");
    let err = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("thread")
        .unwrap_err();
    assert!(err.to_string().contains("image fetch returned 404"));
}

#[tokio::test]
async fn gradio_engine_handles_object_with_url_field() {
    let server = MockServer::start().await;
    let bytes = cheap_payload("object-with-url");
    Mock::given(method("GET"))
        .and(path("/file/from-object.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    let object_url = format!("{}/file/from-object.png", server.uri());
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "url": object_url }],
        })))
        .mount(&server)
        .await;
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some(server.uri()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let task = image_task("object-with-url");
    let result = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("thread")
        .expect("dispatch");
    let got = match result {
        TaskResult::Image { bytes, .. } => bytes,
        other => panic!("expected image, got {:?}", other.kind()),
    };
    assert_eq!(got, bytes);
}

#[tokio::test]
async fn gradio_engine_resolves_relative_url() {
    let server = MockServer::start().await;
    let bytes = cheap_payload("relative");
    Mock::given(method("GET"))
        .and(path("/file/result.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "data": ["/file/result.png"] })),
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
    let task = image_task("relative");
    let result = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("thread")
        .expect("dispatch");
    let got = match result {
        TaskResult::Image { bytes, .. } => bytes,
        other => panic!("expected image, got {:?}", other.kind()),
    };
    assert_eq!(got, bytes);
}

#[tokio::test]
async fn engine_build_errors_for_unknown_engine_name() {
    let cfg = Config {
        engine: "no-such-engine".into(),
        ..Config::default()
    };
    let err = match engine::build(&cfg) {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unknown engine"));
}

#[tokio::test]
async fn engine_build_errors_for_gradio_without_url() {
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: None,
        ..Config::default()
    };
    let err = match engine::build(&cfg) {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("gradio_endpoint_url"));
}

#[tokio::test]
async fn gradio_engine_refuses_llm_tasks() {
    // No HTTP — the engine should reject before making the call.
    let cfg = Config {
        engine: "gradio".into(),
        gradio_endpoint_url: Some("http://example.invalid".into()),
        supported_models_override: vec!["tiny-test".into()],
        ..Config::default()
    };
    let engine = engine::build(&cfg).expect("build engine");
    let task = Task::Llm(LlmParams {
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
        max_tokens: 8,
        temperature: 0.0,
    });
    let err = std::thread::spawn(move || engine.dispatch("tiny-test", task))
        .join()
        .expect("thread")
        .unwrap_err();
    assert!(err.to_string().contains("cannot serve llm"));
}
