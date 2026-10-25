//! `/wake-words` over real HTTP: a training accepted and followed to its end, the refusals by name,
//! the routes' shapes.  Provisioning is faked to fail at once, so nothing is downloaded or trained;
//! the job itself is covered end to end by `wake_word::tests`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use studio_worker::catalog::Catalog;
use studio_worker::engine::torch::platform::ComputeCapability;
use studio_worker::engine::{Engine, SyntheticEngine};
use studio_worker::host::ModelHost;
use studio_worker::job_gate::JobGate;
use studio_worker::local_api::{LocalApi, ModelServices};
use studio_worker::runtime::WorkerObservers;
use studio_worker::test_support::FixedProbe;
use studio_worker::wake_word::{Backend, TrainingError, WakeWords};

const TOKEN: &str = "wake-word-test-token-0123456789";

struct Api {
    url: String,
    gate: JobGate,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _models: tempfile::TempDir,
}

struct Setup {
    gpu: Result<ComputeCapability, String>,
    free: Option<u64>,
    with_wake_words: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            gpu: Ok(ComputeCapability { major: 8, minor: 9 }),
            free: None,
            with_wake_words: true,
        }
    }
}

impl Api {
    fn start(setup: Setup) -> Self {
        let catalog = Arc::new(Mutex::new(Catalog::default()));
        let host = ModelHost::new(
            catalog.clone(),
            Arc::new(studio_worker::test_support::FakeVisionRuntime),
            Arc::new(FixedProbe(20.0)),
            studio_worker::residency::Residency::load_for_serving(None),
        );
        let observers = WorkerObservers::default();
        let gate = JobGate::new();
        let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
        let models = tempfile::tempdir().unwrap();
        let gpu = setup.gpu.clone();
        let free = setup.free;
        let backend = Backend {
            gpu: Box::new(move || gpu.clone()),
            provision: Box::new(|_, _| {
                Err(TrainingError::ProvisionFailed("packages: exit 1".into()))
            }),
            describe_gpu: Box::new(|| None),
            free_space: Box::new(move |_| free),
            datasets: vec![],
            stall: Duration::from_secs(1),
        };
        let mut api = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            catalog,
            None,
            observers.clone(),
            TOKEN.into(),
            gate.clone(),
            None,
            ModelServices::new(host),
        )
        .unwrap();
        if setup.with_wake_words {
            api = api.with_wake_words(WakeWords::with_backend(
                models.path().to_path_buf(),
                observers,
                gate.clone(),
                backend,
            ));
        }
        let url = api.url();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || api.serve(&stop_thread));
        Self {
            url,
            gate,
            stop,
            handle: Some(handle),
            _models: models,
        }
    }

    fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = reqwest::blocking::Client::new()
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(TOKEN);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().unwrap();
        let status = response.status().as_u16();
        (status, response.json().unwrap_or(Value::Null))
    }

    fn post(&self, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, "/wake-words", Some(body))
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, None)
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

fn hey_runa() -> Value {
    json!({
        "phrase": "hey runa",
        "languages": ["en", "nl"],
        "spellings": {"en": ["hey roona"], "nl": ["hej roena", "hey roena"]},
        "negatives": ["hey luna", "runner"]
    })
}

fn ended(api: &Api, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, training) = api.get(&format!("/wake-words/{id}"));
        assert_eq!(status, 200);
        if training["state"] != "running" {
            return training;
        }
        assert!(Instant::now() < deadline, "still running: {training}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_training_is_accepted_followed_and_listed() {
    let api = Api::start(Setup::default());
    let (status, training) = api.post(hey_runa());
    assert_eq!(status, 202, "{training}");
    let id = training["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("local-"), "{id}");
    assert_eq!(training["name"], "hey_runa");
    assert_eq!(training["phrase"], "hey runa");
    assert_eq!(training["state"], "running");
    let output = std::path::PathBuf::from(training["outputDir"].as_str().unwrap());
    assert!(output.ends_with(
        std::path::Path::new("wake-word")
            .join("models")
            .join("hey_runa")
            .join(&id)
    ));

    let training = ended(&api, &id);
    assert_eq!(training["state"], "failed");
    assert_eq!(training["stage"], "provision");
    assert_eq!(training["failure"]["reason"], "provision_failed");
    assert!(training["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("packages: exit 1"));
    assert!(training["finishedAt"].is_string());

    let (status, list) = api.get("/wake-words");
    assert_eq!(status, 200);
    assert_eq!(list[0]["id"], id.as_str());

    let (_, jobs) = api.get("/jobs");
    let job = jobs
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["jobId"] == id.as_str())
        .expect("a local job");
    assert_eq!(job["kind"], "wake_word");
    assert_eq!(job["model"], "microwakeword");

    let (status, body) = api.call(reqwest::Method::DELETE, &format!("/wake-words/{id}"), None);
    assert_eq!((status, body["error"].as_str()), (409, Some("not_running")));
}

#[test]
fn bad_requests_are_refused_by_name() {
    let api = Api::start(Setup::default());
    let (status, body) = api.post(json!({"phrase": "hey runa", "languages": ["xx"]}));
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_request");
    assert!(
        body["message"].as_str().unwrap().contains("languages"),
        "{body}"
    );

    let (status, body) = api.get("/wake-words/nope");
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("unknown_training"))
    );
    let (status, body) = api.call(reqwest::Method::DELETE, "/wake-words/nope", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("unknown_training"))
    );
    let (status, body) = api.call(reqwest::Method::PUT, "/wake-words/nope", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (405, Some("method_not_allowed"))
    );
    let (status, body) = api.get("/wake-words/a/b");
    assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));

    let unauthenticated = reqwest::blocking::Client::new()
        .get(format!("{}/wake-words", api.url))
        .send()
        .unwrap();
    assert_eq!(unauthenticated.status().as_u16(), 401);
}

#[test]
fn a_training_is_refused_when_the_worker_is_busy_or_unfit() {
    let api = Api::start(Setup::default());
    let _held = api.gate.try_reserve().unwrap();
    let (status, body) = api.post(hey_runa());
    assert_eq!((status, body["error"].as_str()), (409, Some("busy")));

    let api = Api::start(Setup {
        gpu: Err("no NVIDIA GPU: nvidia-smi did not answer".into()),
        ..Default::default()
    });
    let (status, body) = api.post(hey_runa());
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("unsupported_platform"))
    );
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("nvidia-smi did not answer"));

    let api = Api::start(Setup {
        free: Some(0),
        ..Default::default()
    });
    let (status, body) = api.post(hey_runa());
    assert_eq!(
        (status, body["error"].as_str()),
        (507, Some("insufficient_disk"))
    );
    assert!(body["neededGib"].as_f64().unwrap() > 10.0, "{body}");
    assert_eq!(body["freeGib"], 0.0);

    let api = Api::start(Setup {
        with_wake_words: false,
        ..Default::default()
    });
    let (status, body) = api.post(hey_runa());
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("wake_words_unavailable"))
    );
}
