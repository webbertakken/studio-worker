//! `/vision/*` over real HTTP: a picture as raw bytes, a form or JSON; the answers' shape; the
//! refusals.  The networks are canned (`FakeVisionRuntime`), so no ONNX Runtime and no GPU.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::Value;
use studio_worker::catalog::Catalog;
use studio_worker::engine::{Engine, SyntheticEngine};
use studio_worker::host::ModelHost;
use studio_worker::job_gate::JobGate;
use studio_worker::lifecycle::ModelState;
use studio_worker::local_api::{LocalApi, ModelServices};
use studio_worker::runtime::{JobSource, WorkerObservers};
use studio_worker::test_support::{vision_catalog, FakeVisionRuntime, FixedProbe};
use studio_worker::types::TaskKind;

const TOKEN: &str = "vision-test-token-0123456789";

struct Api {
    url: String,
    host: ModelHost,
    gate: JobGate,
    observers: WorkerObservers,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Api {
    fn start(catalog: Catalog) -> Self {
        let catalog = Arc::new(Mutex::new(catalog));
        let host = ModelHost::new(
            catalog.clone(),
            Arc::new(FakeVisionRuntime),
            Arc::new(FixedProbe(20.0)),
            studio_worker::residency::Residency::load_for_serving(None),
        );
        let observers = WorkerObservers::default();
        let gate = JobGate::new();
        let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
        let api = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            catalog,
            None,
            observers.clone(),
            TOKEN.into(),
            gate.clone(),
            None,
            ModelServices::new(host.clone()),
        )
        .unwrap();
        let url = api.url();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || api.serve(&stop_thread));
        Self {
            url,
            host,
            gate,
            observers,
            stop,
            handle: Some(handle),
        }
    }

    fn post(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        reqwest::blocking::Client::new()
            .post(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let picture = image::RgbImage::from_pixel(width, height, image::Rgb([90, 120, 60]));
    let mut out = Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .unwrap();
    out.into_inner()
}

fn json(response: reqwest::blocking::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    (status, response.json().unwrap_or(Value::Null))
}

#[test]
fn a_raw_picture_is_searched_for_animals() {
    let api = Api::start(vision_catalog());
    let (status, body) = json(
        api.post("/vision/detect?minConfidence=0.5")
            .header("content-type", "image/jpeg")
            .body(jpeg(64, 64))
            .send()
            .unwrap(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "md");
    assert_eq!(body["device"], "cpu");
    assert_eq!(
        body["picture"],
        serde_json::json!({"width": 64, "height": 64})
    );
    assert_eq!(body["animalPresent"], true);
    let first = &body["detections"][0];
    assert_eq!(first["label"], "animal");
    assert_eq!(first["box"]["x"], 15.0);
    assert_eq!(first["boxNormalised"]["width"], 10.0 / 64.0);
    assert_eq!(body["detections"][1]["animal"], false);
    let job = api.observers.local_jobs.lock()[0].clone();
    assert_eq!(job.kind, TaskKind::Vision);
}

#[test]
fn a_form_picture_is_classified() {
    let api = Api::start(vision_catalog());
    let form = reqwest::blocking::multipart::Form::new()
        .part(
            "image",
            reqwest::blocking::multipart::Part::bytes(jpeg(20, 10))
                .file_name("robin.jpg")
                .mime_str("image/jpeg")
                .unwrap(),
        )
        .text("topK", "2");
    let (status, body) = json(api.post("/vision/classify").multipart(form).send().unwrap());
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "birds");
    let predictions = body["predictions"].as_array().unwrap();
    assert_eq!(predictions.len(), 2);
    assert_eq!(predictions[0]["label"], "robin");
    assert!(predictions[0]["confidence"].as_f64().unwrap() > 0.9);
    assert!(predictions[0].get("taxonomy").is_none());
}

#[test]
fn a_json_picture_has_its_animals_classified() {
    let api = Api::start(vision_catalog());
    let image = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg(64, 64))
    );
    let (status, body) = json(
        api.post("/vision/species")
            .json(&serde_json::json!({"image": image, "detector": "md", "topK": 1}))
            .send()
            .unwrap(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["detector"], "md");
    assert_eq!(body["classifier"], "birds");
    assert_eq!(body["detections"][0]["species"][0]["label"], "robin");
    assert_eq!(
        body["detections"][0]["species"].as_array().unwrap().len(),
        1
    );
    assert!(body["detections"][1].get("species").is_none());
}

#[test]
fn a_loaded_model_answers_while_another_job_holds_the_gate() {
    let api = Api::start(vision_catalog());
    api.host.load("md").unwrap();
    api.host
        .wait_for("md", ModelState::serves, Duration::from_secs(5))
        .expect("loaded");
    let _studio_job = api.gate.try_reserve().unwrap();
    let (status, _) = json(
        api.post("/vision/detect")
            .header("content-type", "image/jpeg")
            .body(jpeg(64, 64))
            .send()
            .unwrap(),
    );
    assert_eq!(status, 200);
    assert_eq!(api.observers.local_jobs.lock()[0].source, JobSource::Lane);

    let response = api
        .post("/vision/classify")
        .header("content-type", "image/jpeg")
        .body(jpeg(8, 8))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 503, "a transient job waits");
    assert_eq!(response.headers()["retry-after"], "2");
    assert_eq!(response.json::<Value>().unwrap()["error"], "busy");
}

#[test]
fn refusals_carry_a_status_and_a_code() {
    let api = Api::start(vision_catalog());
    let raw = |path: &str, content_type: &str, body: Vec<u8>| {
        json(
            api.post(path)
                .header("content-type", content_type)
                .body(body)
                .send()
                .unwrap(),
        )
    };
    let cases = [
        (
            raw("/vision/detect", "image/jpeg", b"junk".to_vec()),
            400,
            "bad_request",
        ),
        (
            raw("/vision/detect", "text/plain", jpeg(4, 4)),
            415,
            "unsupported_media_type",
        ),
        (
            raw("/vision/detect?model=nope", "image/jpeg", jpeg(4, 4)),
            404,
            "unknown_model",
        ),
        (
            raw("/vision/detect?model=birds", "image/jpeg", jpeg(4, 4)),
            400,
            "wrong_model",
        ),
        (
            raw("/vision/classify?iou=0.5", "image/jpeg", jpeg(4, 4)),
            400,
            "bad_request",
        ),
        (
            raw("/vision/classify?topK=0", "image/jpeg", jpeg(4, 4)),
            400,
            "bad_request",
        ),
    ];
    for ((status, body), want_status, want_code) in cases {
        assert_eq!(status, want_status, "{body}");
        assert_eq!(body["error"], want_code, "{body}");
        assert!(body["message"].as_str().is_some_and(|m| !m.is_empty()));
    }
    assert!(api.observers.local_jobs.lock().is_empty(), "nothing ran");
}

#[test]
fn a_catalogue_without_vision_models_answers_no_model() {
    let api = Api::start(Catalog::default());
    let (status, body) = json(
        api.post("/vision/species")
            .header("content-type", "image/jpeg")
            .body(jpeg(4, 4))
            .send()
            .unwrap(),
    );
    assert_eq!((status, body["error"].as_str()), (404, Some("no_model")));
}

#[test]
fn vision_routes_need_the_token() {
    let api = Api::start(vision_catalog());
    let response = reqwest::blocking::Client::new()
        .post(format!("{}/vision/detect", api.url))
        .header("content-type", "image/jpeg")
        .body(jpeg(4, 4))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
}
