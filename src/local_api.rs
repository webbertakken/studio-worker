//! Always-on local HTTP API for image generation (127.0.0.1 only).
//!
//! Synchronous: `POST /image` blocks until the engine finishes and returns the
//! image bytes. Models come from the local [`Catalog`], which the operator can
//! extend at runtime via `POST /models` — the same `ModelSource` shape the
//! studio uses. Every job is recorded into the in-app local queue.
//!
//! ## Security model
//!
//! Binding loopback alone is **not** enough:
//!
//! * Browsers happily fire cross-site `text/plain` POSTs at
//!   `127.0.0.1` without a CORS preflight, and this API parses bodies
//!   as JSON regardless of content type — so without a guard any web
//!   page could inject catalog models (with attacker-controlled
//!   download URLs) or burn the GPU.  DNS rebinding additionally lets
//!   a page *read* responses.
//! * Any other local OS user can reach the port.
//!
//! Defence, checked in order on every route except `GET /healthz`:
//!
//! 1. **Host allow-list** — must be loopback (DNS-rebinding guard).
//! 2. **Origin allow-list** — when present, must be a loopback origin
//!    (CSRF guard; absent means a non-browser client and is allowed).
//! 3. **Bearer token** — `Authorization: Bearer <token>`, compared in
//!    constant time.  The token is generated per install, persisted in
//!    `config.toml`, and published to local clients via the owner-only
//!    `local-api.json` discovery file in the worker's config dir.

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
use crate::job_gate::JobGate;
use crate::local::{run_image, run_kind, LocalError, LocalImageRequest};
use crate::runtime::{JobOutcome, WorkerObservers};
use crate::types::{
    AudioSttParams, AudioTtsParams, ChatMessage, LlmParams, Task, TaskKind, TaskResult, VideoParams,
};

const TRACE_TARGET: &str = "studio_worker::local_api";
const POLL: Duration = Duration::from_millis(200);

/// Maximum accepted request-body size.  `read_body` used to read to
/// string unbounded, so a single request could OOM the worker.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Why a request was denied before reaching its handler.
#[derive(Debug, PartialEq, Eq)]
enum Denial {
    /// `Host` header present but not loopback — DNS rebinding.
    Host(String),
    /// `Origin` header present but not a loopback origin — CSRF.
    Origin(String),
    /// Missing or wrong bearer token.
    Token,
}

/// True when `host` (an HTTP `Host` header value, optionally with a
/// `:port` suffix) names the loopback interface this API binds.
fn host_is_loopback(host: &str) -> bool {
    // `[::1]:port` — bracketed IPv6 keeps its colons, so strip the
    // port only after the closing bracket.
    let bare = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, _port)) => addr,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    bare.eq_ignore_ascii_case("localhost") || bare == "127.0.0.1" || bare == "::1"
}

/// True when an `Origin` header value is a loopback origin.  `null`
/// (sandboxed iframes / redirects) and every remote origin are
/// rejected; only `http(s)://<loopback>[:port]` passes.
fn origin_is_loopback(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    match rest {
        Some(host) => host_is_loopback(host),
        None => false,
    }
}

/// Constant-time bearer-token comparison so a local attacker can't
/// binary-search the token through response timing.
fn token_matches(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The request gate, pure over extracted header values so every
/// branch is unit-testable without a socket.  Checks run in
/// cheapest-and-broadest-first order: Host (rebinding), Origin
/// (CSRF), then the token.
fn deny_reason(
    host: Option<&str>,
    origin: Option<&str>,
    authorization: Option<&str>,
    token: &str,
) -> Option<Denial> {
    if let Some(host) = host {
        if !host_is_loopback(host) {
            return Some(Denial::Host(host.to_string()));
        }
    }
    if let Some(origin) = origin {
        if !origin_is_loopback(origin) {
            return Some(Denial::Origin(origin.to_string()));
        }
    }
    let presented = authorization.and_then(|a| {
        a.strip_prefix("Bearer ")
            .or_else(|| a.strip_prefix("bearer "))
    });
    match presented {
        Some(presented) if token_matches(presented, token) => None,
        _ => Some(Denial::Token),
    }
}

/// The local image API server, bound but not yet serving.
pub struct LocalApi {
    engine: Arc<dyn Engine>,
    catalog: Arc<Mutex<Catalog>>,
    catalog_path: Option<PathBuf>,
    observers: WorkerObservers,
    server: Server,
    addr: SocketAddr,
    /// Bearer token every route except `GET /healthz` requires.
    token: String,
    /// Shared one-job-at-a-time gate.  A local generation reserves it
    /// so it can't run concurrently with a studio job on the same GPU.
    gate: JobGate,
    /// Root the engine downloads models into.  Reported (with its free
    /// space) on `/healthz` so a stuck first-use download is visible.
    models_root: Option<PathBuf>,
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

/// OpenAI-compatible chat-completions request (non-streaming subset).
#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<ChatMessageBody>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stop: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ChatMessageBody {
    role: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TtsBody {
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    ext: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SttBody {
    input_url: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoBody {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    seconds: Option<f32>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    ext: Option<String>,
}

impl LocalApi {
    /// Bind to `addr` (e.g. `127.0.0.1:0` for an ephemeral port).
    // A constructor wiring the API's collaborators (engine, catalog,
    // observers, auth, gate, models-root); grouping them into a struct
    // would only move the argument list, not reduce it.
    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        addr: &str,
        engine: Arc<dyn Engine>,
        catalog: Arc<Mutex<Catalog>>,
        catalog_path: Option<PathBuf>,
        observers: WorkerObservers,
        token: String,
        gate: JobGate,
        models_root: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !token.is_empty(),
            "local api: refusing to serve with an empty token"
        );
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
            token,
            gate,
            models_root,
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
    ///
    /// Requests are handled on a small pool of worker threads (tiny_http's
    /// `Server` is `Sync`, so several threads can `recv_timeout`
    /// concurrently).  A single generation can take ~10 s, and a
    /// first-use model download minutes — on the old single-threaded
    /// loop that blocked `/healthz`, `/models`, and every other caller.
    /// The pool keeps the cheap routes responsive while a job runs.
    /// `std::thread::scope` lets the workers borrow `&self` + `stop`
    /// without an `Arc`, so the public signature is unchanged.
    pub fn serve(&self, stop: &AtomicBool) {
        const WORKERS: usize = 4;
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                scope.spawn(|| {
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
                });
            }
        });
    }

    fn route(&self, request: Request) {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/");

        // `GET /healthz` stays open: liveness only, no secrets.  Every
        // other route passes the Host / Origin / token gate first.
        if !(method == Method::Get && path == "/healthz") {
            let header = |name: &'static str| {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .map(|h| h.value.as_str().to_string())
            };
            let denial = deny_reason(
                header("host").as_deref(),
                header("origin").as_deref(),
                header("authorization").as_deref(),
                &self.token,
            );
            if let Some(denial) = denial {
                let (status, body) = match &denial {
                    Denial::Host(host) => {
                        (403, format!("forbidden: non-loopback Host header {host:?}"))
                    }
                    Denial::Origin(origin) => (
                        403,
                        format!("forbidden: cross-site request from Origin {origin:?}"),
                    ),
                    Denial::Token => (
                        401,
                        "missing or invalid Authorization bearer token; local clients \
                         can read the current token from the local-api.json discovery \
                         file in the worker's config directory"
                            .to_string(),
                    ),
                };
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "deny",
                    method = %method,
                    path,
                    status,
                    reason = ?denial,
                    "local api request denied"
                );
                if let Err(err) = respond(request, status, "text/plain", body.as_bytes()) {
                    tracing::warn!(target: TRACE_TARGET, error = %err, "local api respond error");
                }
                return;
            }
        }

        let outcome = match (&method, path) {
            (Method::Get, "/healthz") => self.handle_healthz(request),
            (Method::Post, "/image") => self.handle_image(request),
            (Method::Post, "/v1/chat/completions") => self.handle_chat(request),
            (Method::Post, "/tts") => self.handle_tts(request),
            (Method::Post, "/stt") => self.handle_stt(request),
            (Method::Post, "/video") => self.handle_video(request),
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

    /// Liveness + a read-only runtime snapshot for operators and local
    /// tooling.  Unauthenticated (no secrets, no prompts): the worker
    /// version, whether a job is in flight, the engine name, and the
    /// models-root free space so a stuck first-use download is
    /// diagnosable without shelling into the box.
    fn handle_healthz(&self, request: Request) -> std::io::Result<()> {
        let free_bytes = self
            .models_root
            .as_deref()
            .and_then(|root| fs4::available_space(root).ok());
        let gpu = self
            .observers
            .gpu_runtime
            .lock()
            .clone()
            .map(|g| serde_json::json!({ "ok": g.ok, "detail": g.detail }));
        let body = serde_json::json!({
            "ok": true,
            "version": crate::AGENT_VERSION,
            "busy": self.gate.is_busy(),
            "engine": self.engine.name(),
            "modelsRoot": self.models_root.as_ref().map(|p| p.display().to_string()),
            "modelsRootFreeBytes": free_bytes,
            "gpuRuntime": gpu,
        });
        match serde_json::to_vec(&body) {
            Ok(bytes) => respond(request, 200, "application/json", &bytes),
            // Never let a serialisation slip break liveness.
            Err(_) => respond(request, 200, "application/json", b"{\"ok\":true}"),
        }
    }

    fn handle_image(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
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

        // One GPU, one job: reserve the shared gate so a local
        // generation never runs alongside a studio job.  Busy → 503 +
        // Retry-After so the caller backs off instead of OOMing.
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };

        let catalog = self.catalog.lock().clone();
        match run_image(self.engine.as_ref(), &catalog, &self.observers, &req) {
            Ok(TaskResult::Image { bytes, ext }) => {
                respond(request, 200, content_type_for(&ext), &bytes)
            }
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-image result"),
            Err(err) => respond_local_err(request, err),
        }
    }

    /// OpenAI-compatible chat completions.  Resolves an LLM model from
    /// the catalog (explicit `model` or the default), dispatches, and
    /// returns the engine's JSON verbatim (the synthetic + llama
    /// engines already emit a `chat.completion`-shaped body).
    fn handle_chat(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: ChatBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad json: {err}").as_bytes(),
                )
            }
        };
        let prompt_preview = parsed
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let params = LlmParams {
            messages: parsed
                .messages
                .into_iter()
                .map(|m| ChatMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
            max_tokens: parsed.max_tokens.unwrap_or(512),
            temperature: parsed.temperature.unwrap_or(0.7),
            top_p: parsed.top_p,
            stop: parsed.stop,
            ..Default::default()
        };
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };
        let catalog = self.catalog.lock().clone();
        match run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::Llm,
            parsed.model.as_deref(),
            &prompt_preview,
            Task::Llm(params),
        ) {
            Ok(TaskResult::Llm { json }) => match serde_json::to_vec(&json) {
                Ok(bytes) => respond(request, 200, "application/json", &bytes),
                Err(e) => respond(request, 500, "text/plain", e.to_string().as_bytes()),
            },
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-llm result"),
            Err(err) => respond_local_err(request, err),
        }
    }

    fn handle_tts(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: TtsBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad json: {err}").as_bytes(),
                )
            }
        };
        let preview = parsed.text.clone();
        let params = AudioTtsParams {
            text: parsed.text,
            voice: parsed.voice.unwrap_or_else(|| "default".into()),
            speed: parsed.speed,
            language: parsed.language,
            ext: parsed.ext.unwrap_or_else(|| "wav".into()),
        };
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };
        let catalog = self.catalog.lock().clone();
        match run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::AudioTts,
            parsed.model.as_deref(),
            &preview,
            Task::AudioTts(params),
        ) {
            Ok(TaskResult::AudioTts { bytes, ext }) => {
                respond(request, 200, content_type_for(&ext), &bytes)
            }
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-audio result"),
            Err(err) => respond_local_err(request, err),
        }
    }

    fn handle_stt(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: SttBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad json: {err}").as_bytes(),
                )
            }
        };
        let preview = parsed.input_url.clone();
        let params = AudioSttParams {
            input_url: parsed.input_url,
            language: parsed.language,
            ..Default::default()
        };
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };
        let catalog = self.catalog.lock().clone();
        match run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::AudioStt,
            parsed.model.as_deref(),
            &preview,
            Task::AudioStt(params),
        ) {
            Ok(TaskResult::AudioStt { json }) => match serde_json::to_vec(&json) {
                Ok(bytes) => respond(request, 200, "application/json", &bytes),
                Err(e) => respond(request, 500, "text/plain", e.to_string().as_bytes()),
            },
            Ok(_) => respond(
                request,
                500,
                "text/plain",
                b"unexpected non-transcript result",
            ),
            Err(err) => respond_local_err(request, err),
        }
    }

    fn handle_video(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: VideoBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    format!("bad json: {err}").as_bytes(),
                )
            }
        };
        let preview = parsed.prompt.clone();
        let params = VideoParams {
            prompt: parsed.prompt,
            negative_prompt: parsed.negative_prompt,
            seconds: parsed.seconds.unwrap_or(2.0),
            width: parsed.width.unwrap_or(256),
            height: parsed.height.unwrap_or(256),
            ext: parsed.ext.unwrap_or_else(|| "mp4".into()),
            ..Default::default()
        };
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };
        let catalog = self.catalog.lock().clone();
        match run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::Video,
            parsed.model.as_deref(),
            &preview,
            Task::Video(params),
        ) {
            Ok(TaskResult::Video { bytes, ext }) => {
                respond(request, 200, content_type_for(&ext), &bytes)
            }
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-video result"),
            Err(err) => respond_local_err(request, err),
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
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
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

/// A request body, or a refusal to read one past [`MAX_BODY_BYTES`].
enum BodyOutcome {
    Ok(String),
    TooLarge,
}

fn read_body(request: &mut Request) -> std::io::Result<BodyOutcome> {
    // Declared length first — reject without reading a byte.
    if matches!(request.body_length(), Some(len) if len > MAX_BODY_BYTES) {
        return Ok(BodyOutcome::TooLarge);
    }
    // Then a hard cap on the reader for chunked / lying senders: read
    // at most one byte past the cap so overflow is detectable.
    let mut body = String::new();
    use std::io::Read as _;
    request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_string(&mut body)?;
    if body.len() > MAX_BODY_BYTES {
        return Ok(BodyOutcome::TooLarge);
    }
    Ok(BodyOutcome::Ok(body))
}

fn respond_too_large(request: Request) -> std::io::Result<()> {
    respond(
        request,
        413,
        "text/plain",
        format!("request body exceeds {MAX_BODY_BYTES} bytes").as_bytes(),
    )
}

/// 503 when the single job slot is taken by another job (studio or
/// local).  Carries `Retry-After: 2` so a client polls back rather
/// than hammering.
fn respond_busy(request: Request) -> std::io::Result<()> {
    let retry = Header::from_bytes(b"Retry-After".as_slice(), b"2".as_slice())
        .expect("static Retry-After header is valid");
    let response = Response::from_data(
        b"worker is busy with another job (studio or local); retry shortly".to_vec(),
    )
    .with_status_code(503)
    .with_header(retry)
    .with_header(
        Header::from_bytes(b"Content-Type".as_slice(), b"text/plain".as_slice())
            .expect("static content-type header is valid"),
    );
    request.respond(response)
}

/// Publish the bound URL + bearer token for local clients, atomically
/// and owner-only (the file carries the token).  Written on every
/// successful bind; removed again by [`remove_discovery_file`] on
/// clean shutdown so stale files can't point at a dead port.
pub fn write_discovery_file(path: &std::path::Path, url: &str, token: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({ "url": url, "token": token });
    let text = serde_json::to_string_pretty(&body)?;
    crate::config::write_atomic(path, text.as_bytes())?;
    tracing::info!(
        target: TRACE_TARGET,
        op = "discovery",
        path = %path.display(),
        url,
        "local api discovery file written"
    );
    Ok(())
}

/// Best-effort removal of the discovery file on shutdown.  A missing
/// file is the desired end state; any other failure is warn-logged so
/// a stale token file never vanishes silently *and* never lingers
/// silently.
pub fn remove_discovery_file(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "discovery",
                path = %path.display(),
                error = %e,
                "failed to remove local api discovery file"
            );
        }
    }
}

fn content_type_for(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// Map a [`LocalError`] onto an HTTP response: catalog/contract errors
/// (unknown or wrong-kind model, none configured) are the caller's
/// fault → 400 with the message; an engine failure is 500.
fn respond_local_err(request: Request, err: LocalError) -> std::io::Result<()> {
    let status = match err {
        LocalError::Engine(_) => 500,
        _ => 400,
    };
    respond(request, status, "text/plain", err.to_string().as_bytes())
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
    use crate::engine::{EngineCapabilities, SyntheticEngine};
    use crate::types::{ModelCliDefaults, ModelEngine, ModelSource, Task, TaskKind};

    /// An engine that sleeps in `dispatch` so a generation stays
    /// in-flight long enough to prove the pool keeps `/healthz`
    /// answering while a job runs.
    struct SlowEngine {
        inner: SyntheticEngine,
        delay: std::time::Duration,
    }

    impl Engine for SlowEngine {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn capabilities(&self) -> EngineCapabilities {
            self.inner.capabilities()
        }
        fn dispatch(&self, model: &str, task: Task) -> anyhow::Result<TaskResult> {
            std::thread::sleep(self.delay);
            self.inner.dispatch(model, task)
        }
    }

    fn synthetic_model_of(id: &str, kind: TaskKind) -> CatalogModel {
        CatalogModel {
            kind,
            ..synthetic_model(id)
        }
    }

    /// A catalog with one synthetic model per kind, so every local
    /// endpoint has a default to resolve.
    fn multi_kind_catalog() -> Catalog {
        Catalog {
            models: vec![
                synthetic_model_of("img", TaskKind::Image),
                synthetic_model_of("chat", TaskKind::Llm),
                synthetic_model_of("tts", TaskKind::AudioTts),
                synthetic_model_of("stt", TaskKind::AudioStt),
                synthetic_model_of("vid", TaskKind::Video),
            ],
        }
    }

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
            origin: "local".into(),
        }
    }

    const TEST_TOKEN: &str = "test-token-0123456789abcdef";

    struct Harness {
        url: String,
        observers: WorkerObservers,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn start(catalog: Catalog) -> Self {
            Self::start_with_gate(catalog, JobGate::new())
        }

        fn start_with_gate(catalog: Catalog, gate: JobGate) -> Self {
            let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
            let observers = WorkerObservers::default();
            let api = LocalApi::bind(
                "127.0.0.1:0",
                engine,
                Arc::new(Mutex::new(catalog)),
                None,
                observers.clone(),
                TEST_TOKEN.to_string(),
                gate.clone(),
                None,
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

        /// Authed POST builder — what a legitimate local client sends.
        fn post(&self, path: &str) -> reqwest::blocking::RequestBuilder {
            reqwest::blocking::Client::new()
                .post(format!("{}{}", self.url, path))
                .bearer_auth(TEST_TOKEN)
        }

        /// Authed GET.
        fn get(&self, path: &str) -> reqwest::blocking::RequestBuilder {
            reqwest::blocking::Client::new()
                .get(format!("{}{}", self.url, path))
                .bearer_auth(TEST_TOKEN)
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

        let res = h
            .post("/image")
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
        let res = h
            .post("/image")
            .json(&serde_json::json!({ "prompt": "x", "ext": "png" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "image/png");
    }

    #[test]
    fn get_models_lists_catalog() {
        let h = Harness::start(seeded_catalog());
        let body = h.get("/models").send().unwrap().text().unwrap();
        assert!(body.contains("synthetic-img"));
    }

    #[test]
    fn post_models_adds_a_model_then_lists_it() {
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/models")
            .json(&synthetic_model("added-model"))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);

        let body = h.get("/models").send().unwrap().text().unwrap();
        assert!(body.contains("added-model"));
    }

    #[test]
    fn unknown_model_is_a_400() {
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/image")
            .json(&serde_json::json!({ "prompt": "x", "model": "nope" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn invalid_json_is_a_400() {
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/image")
            .body("not json")
            .header("content-type", "application/json")
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn healthz_reports_a_runtime_snapshot() {
        let h = Harness::start(seeded_catalog());
        let body: serde_json::Value = reqwest::blocking::get(format!("{}/healthz", h.url))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["version"], crate::AGENT_VERSION);
        assert_eq!(body["busy"], false);
        assert_eq!(body["engine"], "synthetic");
        // No secrets / prompts leak into the unauthenticated snapshot.
        let raw = serde_json::to_string(&body).unwrap();
        assert!(
            !raw.contains(TEST_TOKEN),
            "healthz must not carry the token"
        );
    }

    #[test]
    fn healthz_surfaces_gpu_runtime_when_probed() {
        let h = Harness::start(seeded_catalog());
        // Simulate the startup probe having found a missing runtime.
        crate::runtime::set_gpu_runtime_status(
            &h.observers,
            Err(anyhow::anyhow!(
                "Vulkan runtime not available: install libvulkan1"
            )),
        );
        let body: serde_json::Value = reqwest::blocking::get(format!("{}/healthz", h.url))
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["gpuRuntime"]["ok"], false);
        assert!(body["gpuRuntime"]["detail"]
            .as_str()
            .unwrap()
            .contains("libvulkan1"));
    }

    // -----------------------------------------------------------------
    // Generic per-kind endpoints (chat / tts / stt / video) — the local
    // API serves every modality the worker's engines support, not just
    // image.
    // -----------------------------------------------------------------

    #[test]
    fn chat_completions_returns_an_openai_shaped_body() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({
                "messages": [{"role": "user", "content": "hello there"}],
                "max_tokens": 16
            }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "application/json");
        let body: serde_json::Value = res.json().unwrap();
        // The synthetic engine emits a chat.completion-shaped object.
        assert!(
            body.get("choices").is_some() || body.get("object").is_some(),
            "expected an OpenAI-ish body, got: {body}"
        );
        // The local job was recorded.
        assert!(h
            .observers
            .local_jobs
            .lock()
            .iter()
            .any(|j| j.kind == TaskKind::Llm));
    }

    #[test]
    fn tts_returns_audio_bytes() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/tts")
            .json(&serde_json::json!({ "text": "read this aloud" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "audio/wav");
        assert!(!res.bytes().unwrap().is_empty());
    }

    #[test]
    fn stt_returns_a_transcript_json() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/stt")
            .json(&serde_json::json!({ "inputUrl": "https://example.com/a.wav" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "application/json");
    }

    #[test]
    fn video_returns_bytes() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/video")
            .json(&serde_json::json!({ "prompt": "a tiny dragon" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(!res.bytes().unwrap().is_empty());
    }

    #[test]
    fn chat_without_an_llm_model_is_a_400() {
        // Only an image model in the catalog: a chat request has no
        // model to resolve and must say so (400), not 500.
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
        assert!(res.text().unwrap().contains("llm"));
    }

    #[test]
    fn chat_endpoint_respects_the_busy_gate() {
        let gate = JobGate::new();
        let h = Harness::start_with_gate(multi_kind_catalog(), gate.clone());
        let _held = gate.try_reserve().unwrap();
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({ "messages": [{"role":"user","content":"x"}] }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 503);
    }

    #[test]
    fn jobs_endpoint_reports_after_generation() {
        let h = Harness::start(seeded_catalog());
        h.post("/image")
            .json(&serde_json::json!({ "prompt": "x" }))
            .send()
            .unwrap();
        let body = h.get("/jobs").send().unwrap().text().unwrap();
        assert!(body.contains("\"completed\""));
        assert!(body.contains("synthetic-img"));
    }

    // -----------------------------------------------------------------
    // Auth gate: every route except GET /healthz requires the bearer
    // token; Host / Origin headers must be loopback.  These pin the
    // CSRF / DNS-rebinding / local-user defences end-to-end through a
    // real socket.
    // -----------------------------------------------------------------

    #[test]
    fn routes_reject_requests_without_a_token() {
        let h = Harness::start(seeded_catalog());
        let client = reqwest::blocking::Client::new();
        let cases: Vec<(reqwest::blocking::RequestBuilder, &str)> = vec![
            (
                client
                    .post(format!("{}/image", h.url))
                    .json(&serde_json::json!({ "prompt": "x" })),
                "POST /image",
            ),
            (client.get(format!("{}/models", h.url)), "GET /models"),
            (
                client
                    .post(format!("{}/models", h.url))
                    .json(&synthetic_model("evil")),
                "POST /models",
            ),
            (
                client.delete(format!("{}/models/synthetic-img", h.url)),
                "DELETE /models",
            ),
            (client.get(format!("{}/jobs", h.url)), "GET /jobs"),
        ];
        for (req, name) in cases {
            let res = req.send().unwrap();
            assert_eq!(res.status(), 401, "{name} must require the token");
            let body = res.text().unwrap();
            assert!(
                body.contains("local-api.json"),
                "{name}: the 401 must point at the discovery file, got: {body}"
            );
        }
        // Nothing was mutated by the unauthenticated attempts.
        let body = h.get("/models").send().unwrap().text().unwrap();
        assert!(!body.contains("evil"));
        assert!(body.contains("synthetic-img"));
    }

    #[test]
    fn routes_reject_a_wrong_token() {
        let h = Harness::start(seeded_catalog());
        let res = reqwest::blocking::Client::new()
            .get(format!("{}/models", h.url))
            .bearer_auth("wrong-token")
            .send()
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    #[test]
    fn healthz_needs_no_token() {
        let h = Harness::start(seeded_catalog());
        let res = reqwest::blocking::get(format!("{}/healthz", h.url)).unwrap();
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn healthz_answers_while_a_generation_is_in_flight() {
        // The whole point of the worker pool: a slow (~400 ms) job must
        // not block liveness / cheap routes.  On the old
        // single-threaded loop this `/healthz` would queue behind the
        // generation and only answer after it finished.
        let engine: Arc<dyn Engine> = Arc::new(SlowEngine {
            inner: SyntheticEngine::new(),
            delay: std::time::Duration::from_millis(400),
        });
        let observers = WorkerObservers::default();
        let api = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            Arc::new(Mutex::new(seeded_catalog())),
            None,
            observers,
            TEST_TOKEN.to_string(),
            JobGate::new(),
            None,
        )
        .unwrap();
        let url = api.url();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || api.serve(&stop_thread));

        // Kick off the slow generation on a background thread.
        let gen_url = url.clone();
        let gen = std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .post(format!("{gen_url}/image"))
                .bearer_auth(TEST_TOKEN)
                .json(&serde_json::json!({ "prompt": "slow" }))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .unwrap()
                .status()
                .as_u16()
        });

        // Give the generation time to occupy a worker, then time a
        // /healthz: it must answer well within the generation's 400 ms.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let start = std::time::Instant::now();
        let health = reqwest::blocking::get(format!("{url}/healthz")).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(health.status(), 200);
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "healthz blocked behind the generation ({elapsed:?}); the pool isn't concurrent"
        );
        // While the job runs, healthz reports busy=true.
        let body: serde_json::Value = health.json().unwrap();
        assert_eq!(body["busy"], true, "a running job must show busy=true");

        assert_eq!(gen.join().unwrap(), 200, "the generation still succeeds");
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    #[test]
    fn non_loopback_host_header_is_forbidden_even_with_a_token() {
        // The DNS-rebinding shape: the TCP connection reaches loopback
        // but the browser's Host header names the attacker's domain.
        let h = Harness::start(seeded_catalog());
        let res = h
            .get("/models")
            .header("host", "evil.example:4787")
            .send()
            .unwrap();
        assert_eq!(res.status(), 403);
        assert!(res.text().unwrap().contains("Host"));
    }

    #[test]
    fn cross_site_origin_is_forbidden_even_with_a_token() {
        // The CSRF shape: a browser always attaches the page's Origin
        // to cross-site POSTs.
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/image")
            .header("origin", "https://evil.example")
            .json(&serde_json::json!({ "prompt": "x" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 403);
        assert!(res.text().unwrap().contains("Origin"));
    }

    #[test]
    fn loopback_origin_is_allowed() {
        // A local web app (e.g. a dashboard on localhost:5173) is a
        // legitimate browser client.
        let h = Harness::start(seeded_catalog());
        let res = h
            .get("/models")
            .header("origin", "http://localhost:5173")
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn oversized_body_is_a_413() {
        let h = Harness::start(seeded_catalog());
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let res = h.post("/image").body(big).send().unwrap();
        assert_eq!(res.status(), 413);
    }

    #[test]
    fn body_at_the_cap_is_still_read() {
        // Boundary: exactly MAX_BODY_BYTES must not be rejected as too
        // large (it fails later as bad JSON, which is the point — the
        // size gate stayed out of the way).
        let h = Harness::start(seeded_catalog());
        let exact = "x".repeat(MAX_BODY_BYTES);
        let res = h.post("/image").body(exact).send().unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn bind_refuses_an_empty_token() {
        let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
        let err = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            Arc::new(Mutex::new(seeded_catalog())),
            None,
            WorkerObservers::default(),
            String::new(),
            JobGate::new(),
            None,
        )
        .err()
        .expect("empty token must be refused")
        .to_string();
        assert!(err.contains("empty token"), "got: {err}");
    }

    #[test]
    fn post_image_returns_503_when_the_shared_gate_is_held() {
        // A studio job (or another local job) holds the one-job gate;
        // a concurrent local generation must be refused with 503 +
        // Retry-After rather than run a second job on the same GPU.
        let gate = JobGate::new();
        let h = Harness::start_with_gate(seeded_catalog(), gate.clone());
        let reservation = gate.try_reserve().expect("pre-hold the slot");
        let res = h
            .post("/image")
            .json(&serde_json::json!({ "prompt": "x" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers()["retry-after"], "2");

        // Once the holder releases, the same request succeeds — proving
        // the 503 was the gate, not a broken engine.
        drop(reservation);
        let res = h
            .post("/image")
            .json(&serde_json::json!({ "prompt": "x" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // -----------------------------------------------------------------
    // Pure guards.
    // -----------------------------------------------------------------

    #[test]
    fn host_is_loopback_accepts_only_loopback_shapes() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:4787",
            "localhost",
            "LOCALHOST:80",
            "[::1]",
            "[::1]:4787",
        ] {
            assert!(host_is_loopback(ok), "{ok} should be loopback");
        }
        for bad in [
            "evil.example",
            "evil.example:4787",
            "127.0.0.1.evil.example",
            "192.168.1.10:4787",
            "[::2]:4787",
            "[::1",
            "",
        ] {
            assert!(!host_is_loopback(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn origin_is_loopback_accepts_only_loopback_origins() {
        for ok in [
            "http://127.0.0.1:4787",
            "http://localhost:5173",
            "https://localhost",
            "http://[::1]:3000",
        ] {
            assert!(origin_is_loopback(ok), "{ok} should be allowed");
        }
        for bad in [
            "https://evil.example",
            "http://192.168.1.10",
            "null",
            "file://",
            "chrome-extension://abc",
            "",
        ] {
            assert!(!origin_is_loopback(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn deny_reason_orders_host_origin_then_token() {
        let t = "tok";
        // Bad host wins even when everything else is bad too.
        assert!(matches!(
            deny_reason(Some("evil.example"), Some("https://evil.example"), None, t),
            Some(Denial::Host(_))
        ));
        // Good host, bad origin.
        assert!(matches!(
            deny_reason(Some("127.0.0.1"), Some("https://evil.example"), None, t),
            Some(Denial::Origin(_))
        ));
        // Good host + origin, missing token.
        assert_eq!(
            deny_reason(Some("127.0.0.1"), None, None, t),
            Some(Denial::Token)
        );
        // Malformed authorization schemes are a token failure.
        assert_eq!(
            deny_reason(None, None, Some("Basic dXNlcjpwdw=="), t),
            Some(Denial::Token)
        );
        // Absent host + origin (curl-style) with the right token passes.
        assert_eq!(deny_reason(None, None, Some("Bearer tok"), t), None);
        // Lowercase scheme is tolerated.
        assert_eq!(deny_reason(None, None, Some("bearer tok"), t), None);
    }

    #[test]
    fn token_matches_is_exact() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("", "abc"));
    }

    // -----------------------------------------------------------------
    // Discovery file.
    // -----------------------------------------------------------------

    #[test]
    fn discovery_file_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-api.json");
        write_discovery_file(&path, "http://127.0.0.1:4787", "tok-123").unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["url"], "http://127.0.0.1:4787");
        assert_eq!(parsed["token"], "tok-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "discovery file carries the token and must be owner-only, got {mode:o}"
            );
        }

        remove_discovery_file(&path);
        assert!(!path.exists());
        // Idempotent: removing a missing file is quiet.
        remove_discovery_file(&path);
    }
}
