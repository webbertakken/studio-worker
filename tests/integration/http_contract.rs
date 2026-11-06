//! Integration tests for the surviving HTTP contract.
//!
//! After the WS migration the only worker-side HTTP routes are
//!  - `POST /workers/register` (bootstrap token)
//!  - `POST /workers/:id/jobs/:jobId/complete` (multipart image / audio
//!    / video bytes; only modality that doesn't fit cleanly into WS
//!    frames).
//!
//! Everything else (heartbeat, accept/reject, completeJson, fail, log
//! batches) is now WS frame traffic and is covered in
//! `ws_client_contract.rs` + the orchestrator unit tests on the API
//! side.
use studio_worker::http::{ApiClient, JobOutput};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Run a blocking closure outside the current tokio runtime.
///
/// `reqwest::blocking::Client` spins up its own internal runtime, which
/// panics on drop if it's called from within an enclosing tokio context.
/// Putting the call on a real OS thread sidesteps that.
fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

#[tokio::test]
async fn complete_posts_multipart_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-42/complete"))
        .and(header("authorization", "Bearer tok-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete(
            "w-1",
            "tok-xyz",
            "job-42",
            &JobOutput::new("webp", "a prompt", vec![0xff, 0xd8, 0xff]),
        )
        .unwrap();
    });
}

#[tokio::test]
async fn complete_handles_audio_wav() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-42/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete(
            "w-1",
            "tok-xyz",
            "job-42",
            &JobOutput::new("wav", "a tts prompt", vec![0x52, 0x49, 0x46, 0x46]),
        )
        .unwrap();
    });
}

/// A converted model's report rides the multipart form as `generationMeta`, beside the `.onnx`
/// part (`docs/engines/convert.md`, "Result").
#[tokio::test]
async fn complete_carries_the_generation_meta_of_a_converted_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-7/complete"))
        .and(body_string_contains("name=\"generationMeta\""))
        .and(body_string_contains("{\"kind\":\"model_convert\"}"))
        .and(body_string_contains("filename=\"job-7.onnx\""))
        .and(body_string_contains("name=\"ext\"\r\n\r\nonnx"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        let mut output = JobOutput::new("onnx", "", vec![8, 1, 18]);
        output.generation_meta = Some(r#"{"kind":"model_convert"}"#.into());
        api.complete("w-1", "tok-xyz", "job-7", &output).unwrap();
    });
}

/// A result without metadata sends no `generationMeta` field, so the studio keeps reading the
/// file's own metadata.
#[tokio::test]
async fn complete_sends_no_generation_meta_without_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-8/complete"))
        .and(body_string_contains("generationMeta"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-1/jobs/job-8/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;

    let uri = server.uri();
    detached(move || {
        let api = ApiClient::new(uri).unwrap();
        api.complete(
            "w-1",
            "tok-xyz",
            "job-8",
            &JobOutput::new("webp", "p", vec![1]),
        )
        .unwrap();
    });
}
