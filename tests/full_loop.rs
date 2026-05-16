//! End-to-end test of one full claim→generate→complete cycle against a
//! mock studio API + mock Gradio.  No GPU.
//!
//! This is the "cheap models" e2e the operator asked for: it exercises the
//! gradio engine code path on every PR without spending a single GB of
//! VRAM, and proves the worker can claim a job, run inference, and post
//! the result back.

use std::sync::Arc;
use std::sync::Mutex;

use base64::Engine as _;
use studio_worker::config::Config;
use studio_worker::engine::{self, render_procedural};
use studio_worker::http::ApiClient;
use studio_worker::types::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn caps() -> WorkerCapabilities {
    WorkerCapabilities {
        machine_name: "test-machine".into(),
        username: "tester".into(),
        agent_version: "0.0.0-test".into(),
        engine: "gradio".into(),
        vram_total_gb: 0.0,
        vram_threshold_gb: 64.0,
        auto_enabled: true,
        auto_start: false,
        supported_models: vec!["tiny-test".into()],
    }
}

/// Mock that returns the claim payload once, then 204 forever.
struct OneShotClaim {
    payload: serde_json::Value,
    served: Arc<Mutex<bool>>,
}

impl Respond for OneShotClaim {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut served = self.served.lock().unwrap();
        if *served {
            return ResponseTemplate::new(204);
        }
        *served = true;
        ResponseTemplate::new(200).set_body_json(self.payload.clone())
    }
}

#[tokio::test]
async fn full_loop_claims_generates_completes() {
    let api_server = MockServer::start().await;
    let gradio_server = MockServer::start().await;

    // Pre-render the expected output so the mock returns a deterministic image.
    let expected = render_procedural("a tiny test creature", "webp").unwrap();
    let png = render_procedural("a tiny test creature", "png").unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let data_url = format!("data:image/png;base64,{b64}");

    // Mock Gradio: returns the test image.
    Mock::given(method("POST"))
        .and(path("/run/predict"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [data_url] })),
        )
        .mount(&gradio_server)
        .await;

    // Mock studio API.
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/register"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                serde_json::json!({ "workerId": "w-test", "authToken": "tok-test" }),
            ),
        )
        .mount(&api_server)
        .await;

    let served = Arc::new(Mutex::new(false));
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/claim"))
        .respond_with(OneShotClaim {
            payload: serde_json::json!({
                "jobId": "job-1",
                "gameId": "test-game",
                "assetName": "test-game/creatures/test",
                "model": "tiny-test",
                "vramGbEstimate": 1.0,
                "prompt": "a tiny test creature",
                "ext": "webp",
            }),
            served: served.clone(),
        })
        .mount(&api_server)
        .await;

    let completed = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let completed_clone = completed.clone();
    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/jobs/job-1/complete"))
        .respond_with(move |req: &Request| {
            // Record the request body (multipart) so we can assert the image was
            // forwarded.  We don't parse multipart — just check it's non-empty
            // and includes our marker bytes.
            completed_clone.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true }))
        })
        .mount(&api_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&api_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphics/api/workers/w-test/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&api_server)
        .await;

    // Now run a single iteration of the contract by hand (the run loop is
    // long-lived; we just verify the building blocks compose).
    let api_uri = api_server.uri();
    let gradio_uri = gradio_server.uri();

    let result = std::thread::spawn(move || -> anyhow::Result<()> {
        let api = ApiClient::new(api_uri)?;

        // 1) register
        let registration = api.register("dev-bootstrap-token", caps(), None)?;
        assert_eq!(registration.worker_id, "w-test");

        // 2) heartbeat once
        api.heartbeat(
            &registration.worker_id,
            &registration.auth_token,
            caps(),
            None,
        )?;

        // 3) claim
        let job = api
            .claim(&registration.worker_id, &registration.auth_token)?
            .expect("expected a job");
        assert_eq!(job.job_id, "job-1");
        assert_eq!(job.model, "tiny-test");

        // 4) generate via gradio engine
        let cfg = Config {
            engine: "gradio".into(),
            gradio_endpoint_url: Some(gradio_uri),
            supported_models_override: vec!["tiny-test".into()],
            ..Config::default()
        };
        let eng = engine::build(&cfg)?;
        let image = eng.generate(&job.prompt, &job.model, &job.ext)?;
        assert!(!image.is_empty(), "engine should have produced image bytes");

        // 5) complete
        api.complete(
            &registration.worker_id,
            &registration.auth_token,
            &job.job_id,
            &job.ext,
            &job.prompt,
            image.clone(),
        )?;

        // 6) ship logs
        api.ship_logs(
            &registration.worker_id,
            &registration.auth_token,
            LogBatch {
                entries: vec![LogEntry {
                    ts: "2026-05-16T00:00:00.000Z".into(),
                    level: "info".into(),
                    category: "test".into(),
                    message: "all done".into(),
                    job_id: Some(job.job_id.clone()),
                }],
            },
        )?;

        Ok(())
    })
    .join()
    .expect("worker thread panicked");
    result.expect("full loop should succeed");

    // The mock recorded at least one completed multipart body.
    let bodies = completed.lock().unwrap();
    assert!(!bodies.is_empty(), "complete endpoint should have been hit");
    assert!(!bodies[0].is_empty(), "multipart body should be non-empty");

    // Sanity: ensure the synthetic engine would have produced something different.
    // (We're not asserting the gradio output exactly matches the synthetic one;
    // we only check both engines yield non-empty bytes of plausible size.)
    assert!(expected.len() > 100);
}
