//! Always-on local HTTP API for image generation (127.0.0.1 only, no auth).
//!
//! Synchronous: `POST /image` blocks until the engine finishes and returns the
//! image bytes. Models come from the local [`Catalog`], which the operator can
//! extend at runtime via `POST /models` — the same `ModelSource` shape the
//! studio uses. Every job is recorded into the in-app local queue.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::catalog::{Catalog, CatalogModel};
use crate::engine::Engine;
use crate::local::{run_image, LocalError, LocalImageRequest};
use crate::runtime::{JobOutcome, WorkerObservers};
use crate::types::TaskResult;

const TRACE_TARGET: &str = "studio_worker::local_api";
const POLL: Duration = Duration::from_millis(200);

/// The local image API server, bound but not yet serving.
pub struct LocalApi {
    engine: Arc<dyn Engine>,
    catalog: Arc<Mutex<Catalog>>,
    catalog_path: Option<PathBuf>,
    observers: WorkerObservers,
    server: Server,
    addr: SocketAddr,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageBody {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    steps: Option<u32>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    ext: Option<String>,
}

impl LocalApi {
    /// Bind to `addr` (e.g. `127.0.0.1:0` for an ephemeral port).
    pub fn bind(
        addr: &str,
        engine: Arc<dyn Engine>,
        catalog: Arc<Mutex<Catalog>>,
        catalog_path: Option<PathBuf>,
        observers: WorkerObservers,
    ) -> anyhow::Result<Self> {
        let server =
            Server::http(addr).map_err(|e| anyhow::anyhow!("local api bind {addr}: {e}"))?;
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow::anyhow!("local api: non-ip listen address"))?;
        Ok(Self {
            engine,
            catalog,
            catalog_path,
            observers,
            server,
            addr,
        })
    }

    /// The bound socket address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL the API is reachable at.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Serve requests until `stop` is set.
    pub fn serve(&self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match self.server.recv_timeout(POLL) {
                Ok(Some(request)) => self.route(request),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(target: TRACE_TARGET, error = %err, "local api recv error");
                    break;
                }
            }
        }
    }

    fn route(&self, request: Request) {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/");

        let outcome = match (&method, path) {
            (Method::Get, "/healthz") => {
                respond(request, 200, "application/json", b"{\"ok\":true}")
            }
            (Method::Post, "/image") => self.handle_image(request),
            (Method::Get, "/models") => self.handle_list_models(request),
            (Method::Post, "/models") => self.handle_add_model(request),
            (Method::Get, "/jobs") => self.handle_jobs(request),
            (Method::Delete, p) if p.starts_with("/models/") => {
                let id = p.trim_start_matches("/models/").to_string();
                self.handle_delete_model(request, &id)
            }
            _ => respond(request, 404, "text/plain", b"not found"),
        };
        if let Err(err) = outcome {
            tracing::warn!(target: TRACE_TARGET, error = %err, "local api respond error");
        }
    }

    fn handle_image(&self, mut request: Request) -> std::io::Result<()> {
        let body = read_body(&mut request)?;
        let parsed: ImageBody = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad json: {err}").as_bytes(),
                )
            }
        };
        let req = LocalImageRequest {
            prompt: parsed.prompt,
            model: parsed.model,
            negative_prompt: parsed.negative_prompt,
            width: parsed.width,
            height: parsed.height,
            steps: parsed.steps,
            seed: parsed.seed,
            ext: parsed.ext,
        };

        let catalog = self.catalog.lock().clone();
        match run_image(self.engine.as_ref(), &catalog, &self.observers, &req) {
            Ok(TaskResult::Image { bytes, ext }) => {
                respond(request, 200, content_type_for(&ext), &bytes)
            }
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-image result"),
            Err(err) => {
                let status = match err {
                    LocalError::Engine(_) => 500,
                    _ => 400,
                };
                respond(request, status, "text/plain", err.to_string().as_bytes())
            }
        }
    }

    fn handle_list_models(&self, request: Request) -> std::io::Result<()> {
        let catalog = self.catalog.lock();
        match serde_json::to_vec(&catalog.models) {
            Ok(body) => respond(request, 200, "application/json", &body),
            Err(err) => respond(request, 500, "text/plain", err.to_string().as_bytes()),
        }
    }

    fn handle_add_model(&self, mut request: Request) -> std::io::Result<()> {
        let body = read_body(&mut request)?;
        let model: CatalogModel = match serde_json::from_str(&body) {
            Ok(model) => model,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad model: {err}").as_bytes(),
                )
            }
        };
        let saved = {
            let mut catalog = self.catalog.lock();
            catalog.upsert(model);
            self.persist(&catalog)
        };
        match saved {
            Ok(()) => respond(request, 200, "application/json", b"{\"ok\":true}"),
            Err(err) => respond(request, 500, "text/plain", err.to_string().as_bytes()),
        }
    }

    fn handle_delete_model(&self, request: Request, id: &str) -> std::io::Result<()> {
        let (existed, saved) = {
            let mut catalog = self.catalog.lock();
            let existed = catalog.remove(id);
            (existed, self.persist(&catalog))
        };
        if !existed {
            return respond(request, 404, "text/plain", b"no such model");
        }
        match saved {
            Ok(()) => respond(request, 200, "application/json", b"{\"ok\":true}"),
            Err(err) => respond(request, 500, "text/plain", err.to_string().as_bytes()),
        }
    }

    fn handle_jobs(&self, request: Request) -> std::io::Result<()> {
        let jobs: Vec<serde_json::Value> = self
            .observers
            .local_jobs
            .lock()
            .iter()
            .map(|job| {
                let (status, reason) = match &job.outcome {
                    JobOutcome::Completed => ("completed", None),
                    JobOutcome::Failed { reason } => ("failed", Some(reason.clone())),
                };
                serde_json::json!({
                    "jobId": job.job_id,
                    "kind": job.kind.as_str(),
                    "model": job.model,
                    "prompt": job.prompt,
                    "status": status,
                    "reason": reason,
                    "startedAt": job.started_at.to_rfc3339(),
                    "finishedAt": job.finished_at.to_rfc3339(),
                })
            })
            .collect();
        match serde_json::to_vec(&jobs) {
            Ok(body) => respond(request, 200, "application/json", &body),
            Err(err) => respond(request, 500, "text/plain", err.to_string().as_bytes()),
        }
    }

    fn persist(&self, catalog: &Catalog) -> std::io::Result<()> {
        match &self.catalog_path {
            Some(path) => catalog.save(path),
            None => Ok(()),
        }
    }
}

fn read_body(request: &mut Request) -> std::io::Result<String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    Ok(body)
}

fn content_type_for(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

fn respond(request: Request, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let header = Header::from_bytes(b"Content-Type".as_slice(), content_type.as_bytes())
        .expect("static content-type header is valid");
    let response = Response::from_data(body)
        .with_status_code(status)
        .with_header(header);
    request.respond(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogModel;
    use crate::engine::SyntheticEngine;
    use crate::types::{ModelCliDefaults, ModelEngine, ModelSource, TaskKind};

    fn synthetic_model(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            kind: TaskKind::Image,
            vram_gb_estimate: 0.0,
            description: None,
            source: ModelSource {
                engine: ModelEngine::Synthetic,
                files: vec![],
                cli_defaults: ModelCliDefaults {
                    cfg_scale: 1.0,
                    steps: 4,
                    width: 64,
                    height: 64,
                    ..Default::default()
                },
            },
            enabled: true,
        }
    }

    struct Harness {
        url: String,
        observers: WorkerObservers,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn start(catalog: Catalog) -> Self {
            let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
            let observers = WorkerObservers::default();
            let api = LocalApi::bind(
                "127.0.0.1:0",
                engine,
                Arc::new(Mutex::new(catalog)),
                None,
                observers.clone(),
            )
            .unwrap();
            let url = api.url();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = std::thread::spawn(move || api.serve(&stop_thread));
            Harness {
                url,
                observers,
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn seeded_catalog() -> Catalog {
        Catalog {
            models: vec![synthetic_model("synthetic-img")],
        }
    }

    #[test]
    fn post_image_returns_image_bytes_and_records_job() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();

        let res = client
            .post(format!("{}/image", h.url))
            .json(&serde_json::json!({ "prompt": "a blue bird" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "image/webp");
        let bytes = res.bytes().unwrap();
        assert!(!bytes.is_empty());

        assert_eq!(h.observers.local_jobs.lock().len(), 1);
    }

    #[test]
    fn post_image_honours_requested_ext() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        let res = client
            .post(format!("{}/image", h.url))
            .json(&serde_json::json!({ "prompt": "x", "ext": "png" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "image/png");
    }

    #[test]
    fn get_models_lists_catalog() {
        let h = Harness::start(seeded_catalog());
        let body = reqwest::blocking::get(format!("{}/models", h.url))
            .unwrap()
            .text()
            .unwrap();
        assert!(body.contains("synthetic-img"));
    }

    #[test]
    fn post_models_adds_a_model_then_lists_it() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        let res = client
            .post(format!("{}/models", h.url))
            .json(&synthetic_model("added-model"))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);

        let body = reqwest::blocking::get(format!("{}/models", h.url))
            .unwrap()
            .text()
            .unwrap();
        assert!(body.contains("added-model"));
    }

    #[test]
    fn unknown_model_is_a_400() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        let res = client
            .post(format!("{}/image", h.url))
            .json(&serde_json::json!({ "prompt": "x", "model": "nope" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn invalid_json_is_a_400() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        let res = client
            .post(format!("{}/image", h.url))
            .body("not json")
            .header("content-type", "application/json")
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn healthz_ok() {
        let h = Harness::start(seeded_catalog());
        let res = reqwest::blocking::get(format!("{}/healthz", h.url)).unwrap();
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn jobs_endpoint_reports_after_generation() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        client
            .post(format!("{}/image", h.url))
            .json(&serde_json::json!({ "prompt": "x" }))
            .send()
            .unwrap();
        let body = reqwest::blocking::get(format!("{}/jobs", h.url))
            .unwrap()
            .text()
            .unwrap();
        assert!(body.contains("\"completed\""));
        assert!(body.contains("synthetic-img"));
    }
}
