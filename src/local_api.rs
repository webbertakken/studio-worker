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
use crate::host::{HostError, ModelHost, ModelStatus};
use crate::job_gate::JobGate;
use crate::lifecycle::ModelState;
use crate::local::{chat_on_lane, run_image, run_kind, LocalError, LocalImageRequest};
use crate::runtime::{JobOutcome, WorkerObservers};
use crate::stt_stream::server::NoLanAddress;
use crate::stt_stream::tokens::StreamTokens;
use crate::types::{
    AudioSttParams, AudioTtsParams, ChatMessage, LlmParams, Task, TaskKind, TaskResult, VideoParams,
};

mod mesh;
mod picture;
mod vision;
mod voice_samples;
mod wake_word;

const TRACE_TARGET: &str = "studio_worker::local_api";
const POLL: Duration = Duration::from_millis(200);

/// Maximum accepted request-body size.  `read_body` used to read to
/// string unbounded, so a single request could OOM the worker.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Maximum body of `POST /image`, which may carry inline input images (a base, a reference and a
/// mask as base64): a 4K PNG base is about 25 MiB, a third larger in base64.
pub const MAX_IMAGE_BODY_BYTES: usize = 64 * 1024 * 1024;

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
    /// The model host and streaming tokens.
    services: ModelServices,
    /// The daemon's runtime handles, for the `/daemon/*` routes.  `None`
    /// answers them `503 daemon_control_unavailable`.
    control: Option<crate::control::DaemonControl>,
    /// Wake word trainings, for the `/wake-words` routes.  `None` answers them
    /// `503 wake_words_unavailable`.
    wake_words: Option<crate::wake_word::WakeWords>,
}

/// What the local API offers besides one-off jobs: the model host, and the
/// tokens that open streaming sessions on the LAN listener.
#[derive(Clone)]
pub struct ModelServices {
    pub host: ModelHost,
    pub tokens: Arc<StreamTokens>,
    /// The running LAN stream listener's port and pinned certificate;
    /// `None` while it is not listening.
    pub stream_endpoint:
        Arc<parking_lot::RwLock<Option<crate::stt_stream::server::StreamEndpoint>>>,
    /// The machine's LAN address now, asked afresh for every grant so a new
    /// DHCP lease reaches the next one.
    pub lan_address: LanAddressSource,
    /// Recording tokens: their own store, so they upload takes and nothing else.
    pub voice_tokens: Arc<StreamTokens>,
    /// The operator's recordings; `None` answers their routes `503 voice_samples_unavailable`.
    pub voice_samples: Option<crate::voice_samples::VoiceSamples>,
}

/// Where a grant learns the machine's LAN address.
pub type LanAddressSource = Arc<dyn Fn() -> Result<std::net::Ipv4Addr, NoLanAddress> + Send + Sync>;

impl ModelServices {
    pub fn new(host: ModelHost) -> Self {
        Self {
            host,
            tokens: Arc::new(StreamTokens::default()),
            stream_endpoint: Arc::new(parking_lot::RwLock::new(None)),
            lan_address: Arc::new(|| {
                crate::stt_stream::server::lan_address(crate::stt_stream::server::route_source())
            }),
            voice_tokens: Arc::new(StreamTokens::default()),
            voice_samples: None,
        }
    }

    /// Keep the operator's recordings in `store`.
    pub fn with_voice_samples(mut self, store: crate::voice_samples::VoiceSamples) -> Self {
        self.voice_samples = Some(store);
        self
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamTokenBody {
    model: String,
    #[serde(default)]
    ttl_secs: Option<i64>,
}

/// A stream token lives this long unless the request says otherwise.
/// The holder (Runa) refreshes well before expiry.  Safe range: within
/// the token store's clamp (30 s..=1 h).
const DEFAULT_STREAM_TOKEN_TTL_SECS: i64 = 600;

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
    #[serde(default)]
    init_image_url: Option<String>,
    #[serde(default)]
    ref_image_url: Option<String>,
    #[serde(default)]
    mask_url: Option<String>,
    #[serde(default)]
    denoise: Option<f32>,
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
    /// llama-server compatible template switches (e.g. `enable_thinking`).
    #[serde(default)]
    chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
    /// Stream the answer as server-sent events (a loaded model only).
    #[serde(default)]
    stream: bool,
    /// How long the caller will wait, from arrival.  Past it a queued
    /// chat is dropped and a running one stopped (`504`).
    #[serde(default)]
    deadline_ms: Option<u64>,
    /// Pictures the model reads with the last user message, as inline
    /// `data:image/...;base64,` URLs (the studio's `images`).
    #[serde(default)]
    images: Vec<String>,
    /// OpenAI's structured output: `{"type":"json_schema","json_schema":{"schema":…}}`
    /// constrains the answer to the schema; `{"type":"text"}` leaves it free.
    #[serde(default)]
    response_format: Option<serde_json::Value>,
}

/// The JSON schema an OpenAI `response_format` asks for: `json_schema` gives its `schema`,
/// `text` none; anything else is refused by name.
fn schema_of(
    response_format: Option<serde_json::Value>,
) -> Result<Option<serde_json::Value>, LocalError> {
    let Some(format) = response_format else {
        return Ok(None);
    };
    match format["type"].as_str() {
        Some("text") => Ok(None),
        Some("json_schema") => match format["json_schema"].get("schema") {
            Some(schema) if schema.is_object() => Ok(Some(schema.clone())),
            _ => Err(LocalError::BadInput(
                "response_format json_schema needs json_schema.schema, an object".into(),
            )),
        },
        other => Err(LocalError::BadInput(format!(
            "response_format type {} is not served; use json_schema or text",
            other.map_or_else(|| "(missing)".to_string(), |t| format!("`{t}`"))
        ))),
    }
}

/// llama-server compatible `/tokenize` request.
#[derive(Deserialize)]
struct TokenizeBody {
    content: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    add_special: bool,
}

/// Chunks the streaming thread sends ahead of a slow client.  Bounded so a
/// stalled client applies back-pressure to generation.
const STREAM_BUFFER_CHUNKS: usize = 256;

/// A response body fed by a channel: each message is written as it comes;
/// the body ends when the sender is dropped.
struct ChannelBody {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ChannelBody {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.buf.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buf = next;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
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
        services: ModelServices,
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
            services,
            control: None,
            wake_words: None,
        })
    }

    /// Serve the `/daemon/*` routes with `control`.
    pub fn with_control(mut self, control: crate::control::DaemonControl) -> Self {
        self.control = Some(control);
        self
    }

    /// Serve the `/wake-words` routes with `wake_words`.
    pub fn with_wake_words(mut self, wake_words: crate::wake_word::WakeWords) -> Self {
        self.wake_words = Some(wake_words);
        self
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
        // Long generations each hold a thread while the tray UI polls once a
        // second; eight keeps the cheap routes answering alongside them.
        const WORKERS: usize = 8;
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
            (Method::Post, "/tokenize") => self.handle_tokenize(request),
            (Method::Post, "/tts") => self.handle_tts(request),
            (Method::Post, "/stt") => self.handle_stt(request),
            (Method::Post, "/video") => self.handle_video(request),
            (Method::Post, "/mesh") => self.handle_mesh(request),
            (Method::Get, "/models") => self.handle_list_models(request),
            (Method::Post, "/models") => self.handle_add_model(request),
            (Method::Get, "/jobs") => self.handle_jobs(request),
            (Method::Post, "/stream-tokens") => self.handle_stream_token(request),
            (_, p) if voice_samples::is_route(p) => {
                let path = p.to_string();
                self.handle_voice_samples(request, &method, &path)
            }
            (_, p) if wake_word::is_route(p) => {
                let path = p.to_string();
                self.handle_wake_words(request, &method, &path)
            }
            (Method::Post, p) if vision::Route::of(p).is_some() => {
                let route = vision::Route::of(p).expect("matched just above");
                self.handle_vision(request, route)
            }
            (_, p) if p.starts_with("/daemon/") => self.handle_daemon(request, &method, &url),
            (Method::Get, p) if job_route(p, "/log").is_some() => {
                let id = job_route(p, "/log").unwrap_or_default().to_string();
                self.handle_job_log(request, &id)
            }
            (Method::Get, p) if job_route(p, "/thumbnail").is_some() => {
                let id = job_route(p, "/thumbnail").unwrap_or_default().to_string();
                self.handle_job_thumbnail(request, &id)
            }
            (Method::Get, p) if lifecycle_route(p, "/state").is_some() => {
                let id = lifecycle_route(p, "/state").unwrap_or_default().to_string();
                self.respond_lifecycle(request, self.services.host.status(&id), 200)
            }
            (Method::Post, p) if lifecycle_route(p, "/load").is_some() => {
                let id = lifecycle_route(p, "/load").unwrap_or_default().to_string();
                self.respond_lifecycle(request, self.services.host.load(&id), 202)
            }
            (Method::Post, p) if lifecycle_route(p, "/unload").is_some() => {
                let id = lifecycle_route(p, "/unload")
                    .unwrap_or_default()
                    .to_string();
                self.respond_lifecycle(request, self.services.host.unload(&id), 202)
            }
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
        let body = match read_body_capped(&mut request, MAX_IMAGE_BODY_BYTES)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => {
                return respond_too_large_capped(request, MAX_IMAGE_BODY_BYTES)
            }
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
            init_image_url: parsed.init_image_url,
            ref_image_url: parsed.ref_image_url,
            mask_url: parsed.mask_url,
            denoise: parsed.denoise,
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
                respond(request, 200, crate::http::mime_for_ext(&ext), &bytes)
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
        let received = std::time::Instant::now();
        let body = match read_body_capped(&mut request, MAX_IMAGE_BODY_BYTES)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => {
                return respond_too_large_capped(request, MAX_IMAGE_BODY_BYTES)
            }
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
        let catalog = self.catalog.lock().clone();
        if let Err(err) = chat_images_readable(&catalog, parsed.model.as_deref(), &parsed.images) {
            return respond_local_err(request, err);
        }
        let json_schema = match schema_of(parsed.response_format) {
            Ok(schema) => schema,
            Err(err) => return respond_local_err(request, err),
        };
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
            chat_template_kwargs: parsed.chat_template_kwargs,
            images: parsed.images,
            json_schema,
            ..Default::default()
        };
        let deadline = parsed
            .deadline_ms
            .and_then(|ms| received.checked_add(std::time::Duration::from_millis(ms)));
        if parsed.stream {
            return self.stream_chat(
                request,
                &catalog,
                parsed.model.as_deref(),
                &prompt_preview,
                params,
                deadline,
            );
        }
        // A loaded model answers on its own lane, outside the job gate.
        if let Some(result) = chat_on_lane(
            &self.services.host,
            &catalog,
            &self.observers,
            parsed.model.as_deref(),
            &prompt_preview,
            params.clone(),
            deadline,
        ) {
            return respond_llm(request, result);
        }
        let Some(_reservation) = self.gate.try_reserve() else {
            return respond_busy(request);
        };
        let outcome = run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::Llm,
            parsed.model.as_deref(),
            &prompt_preview,
            Task::Llm(params),
        );
        respond_llm(request, outcome)
    }

    /// Stream a chat from a loaded model as OpenAI `chat.completion.chunk`
    /// server-sent events, ending with `data: [DONE]`.
    fn stream_chat(
        &self,
        request: Request,
        catalog: &Catalog,
        model_id: Option<&str>,
        prompt_preview: &str,
        params: LlmParams,
        deadline: Option<std::time::Instant>,
    ) -> std::io::Result<()> {
        let model = match crate::local::resolve_llm(catalog, model_id) {
            Ok(model) => model.id.clone(),
            Err(err) => return respond_local_err(request, err),
        };
        if let Err(refusal) = self.require_loaded(&model) {
            return respond_json(request, 409, &refusal);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STREAM_BUFFER_CHUNKS);
        let host = self.services.host.clone();
        let observers = self.observers.clone();
        let preview = prompt_preview.to_string();
        std::thread::spawn(move || {
            crate::local::stream_on_lane(
                &host,
                &observers,
                &model,
                &preview,
                params,
                deadline,
                &mut |bytes| tx.send(bytes).is_ok(),
            );
        });
        let body = ChannelBody {
            rx,
            buf: Vec::new(),
            pos: 0,
        };
        let headers = vec![
            Header::from_bytes("content-type", "text/event-stream").expect("static header"),
            Header::from_bytes("cache-control", "no-cache").expect("static header"),
        ];
        request.respond(Response::new(200.into(), headers, body, None, None))
    }

    /// `Ok` when `model` is loaded; else the `model_not_loaded` refusal body.
    fn require_loaded(&self, model: &str) -> Result<(), serde_json::Value> {
        match self.services.host.status(model) {
            Ok(status) if status.state.serves() => Ok(()),
            Ok(status) => Err(serde_json::json!({
                "error": "model_not_loaded",
                "state": status.state.name(),
                "message": format!("model {model} is not loaded; POST /models/{model}/load first"),
            })),
            Err(err) => {
                Err(serde_json::json!({ "error": "model_not_loaded", "message": err.to_string() }))
            }
        }
    }

    /// llama-server compatible token count: `{content, model?, add_special?}`
    /// -> `{tokens: [...]}` from a loaded chat model.
    fn handle_tokenize(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: TokenizeBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond_json(
                    request,
                    400,
                    &serde_json::json!({ "error": "bad_request", "message": err.to_string() }),
                )
            }
        };
        let model = {
            let catalog = self.catalog.lock();
            match crate::local::resolve_llm(&catalog, parsed.model.as_deref()) {
                Ok(model) => model.id.clone(),
                Err(err @ LocalError::UnknownModel(_)) => {
                    return respond_json(
                        request,
                        404,
                        &serde_json::json!({ "error": "unknown_model", "message": err.to_string() }),
                    )
                }
                Err(err) => {
                    return respond_json(
                        request,
                        400,
                        &serde_json::json!({ "error": "bad_request", "message": err.to_string() }),
                    )
                }
            }
        };
        if let Err(refusal) = self.require_loaded(&model) {
            return respond_json(request, 409, &refusal);
        }
        match crate::local::tokenize_on_lane(
            &self.services.host,
            &model,
            &parsed.content,
            parsed.add_special,
        ) {
            Ok(Ok(tokens)) => respond_json(request, 200, &serde_json::json!({ "tokens": tokens })),
            Ok(Err(err)) => respond_json(
                request,
                500,
                &serde_json::json!({ "error": "tokenize_failed", "message": format!("{err:#}") }),
            ),
            Err(err) => respond_json(
                request,
                409,
                &serde_json::json!({ "error": "model_not_loaded", "message": err.to_string() }),
            ),
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
                respond(request, 200, crate::http::mime_for_ext(&ext), &bytes)
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
                respond(request, 200, crate::http::mime_for_ext(&ext), &bytes)
            }
            Ok(_) => respond(request, 500, "text/plain", b"unexpected non-video result"),
            Err(err) => respond_local_err(request, err),
        }
    }

    fn handle_list_models(&self, request: Request) -> std::io::Result<()> {
        let models = self.catalog.lock().models.clone();
        let statuses = self.services.host.statuses();
        let listed: Vec<serde_json::Value> = models
            .iter()
            .map(|model| {
                let mut value = serde_json::to_value(model).unwrap_or_default();
                if let Some(obj) = value.as_object_mut() {
                    obj.insert(
                        "images".into(),
                        crate::vision::reads_images(&model.source).into(),
                    );
                }
                if let (Some(obj), Some(status)) = (
                    value.as_object_mut(),
                    statuses.iter().find(|s| s.id == model.id),
                ) {
                    obj.insert("state".into(), status.state.name().into());
                    obj.insert("resident".into(), status.resident.into());
                    obj.insert("since".into(), status.since.to_rfc3339().into());
                    obj.insert("loadable".into(), self.services.host.can_load(model).into());
                    if let ModelState::Failed { reason } = &status.state {
                        obj.insert("error".into(), reason.clone().into());
                    }
                }
                value
            })
            .collect();
        match serde_json::to_vec(&listed) {
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
        // Free the weights and drop the residency before the entry goes.
        if let Err(err) = self.services.host.unload(id) {
            if !matches!(err, HostError::UnknownModel(_)) {
                return respond_json(
                    request,
                    500,
                    &serde_json::json!({ "error": "unload_failed", "message": err.to_string() }),
                );
            }
        }
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

    /// Answer a lifecycle call: the model's status, or a named error.
    /// `pending_status` is used while the model is still transitioning.
    fn respond_lifecycle(
        &self,
        request: Request,
        outcome: Result<ModelStatus, HostError>,
        pending_status: u16,
    ) -> std::io::Result<()> {
        match outcome {
            Ok(status) => {
                let code = match status.state {
                    ModelState::Loading | ModelState::Unloading => pending_status,
                    _ => 200,
                };
                respond_json(request, code, &status_json(&status))
            }
            Err(err) => {
                let (code, body) = match &err {
                    HostError::UnknownModel(_) => {
                        (404, serde_json::json!({ "error": "unknown_model" }))
                    }
                    HostError::Disabled(_) => {
                        (400, serde_json::json!({ "error": "model_disabled" }))
                    }
                    HostError::Refused(r) => (
                        409,
                        serde_json::json!({
                            "error": "insufficient_memory",
                            "neededGib": r.needed_gib,
                            "freeGib": r.free_gib,
                            "marginGib": r.margin_gib,
                        }),
                    ),
                    HostError::NotLoaded { state, .. } => (
                        409,
                        serde_json::json!({ "error": "model_not_loaded", "state": state }),
                    ),
                    HostError::Persist(_) => {
                        (500, serde_json::json!({ "error": "residency_not_saved" }))
                    }
                    HostError::LaneBusy(_) => (409, serde_json::json!({ "error": "model_busy" })),
                };
                let mut body = body;
                body["message"] = err.to_string().into();
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "lifecycle",
                    status = code,
                    error = %err,
                    "lifecycle request refused"
                );
                respond_json(request, code, &body)
            }
        }
    }

    /// Mint a short-lived token that opens one streaming model on the LAN
    /// listener (\`ws://<host>:<port>/transcribe?token=...\`).
    fn handle_stream_token(&self, mut request: Request) -> std::io::Result<()> {
        let body = match read_body(&mut request)? {
            BodyOutcome::Ok(body) => body,
            BodyOutcome::TooLarge => return respond_too_large(request),
        };
        let parsed: StreamTokenBody = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(err) => {
                return respond_json(
                    request,
                    400,
                    &serde_json::json!({ "error": "bad_request", "message": err.to_string() }),
                )
            }
        };
        let model = self.catalog.lock().get(&parsed.model).cloned();
        let Some(model) = model else {
            return respond_json(
                request,
                404,
                &serde_json::json!({ "error": "unknown_model" }),
            );
        };
        if model.source.engine != crate::types::ModelEngine::Parakeet {
            return respond_json(
                request,
                400,
                &serde_json::json!({ "error": "not_a_stream_model" }),
            );
        }
        let endpoint = self.services.stream_endpoint.read().clone();
        let Some(endpoint) = endpoint else {
            return respond_json(
                request,
                503,
                &serde_json::json!({ "error": "stream_listener_down" }),
            );
        };
        let ttl =
            chrono::Duration::seconds(parsed.ttl_secs.unwrap_or(DEFAULT_STREAM_TOKEN_TTL_SECS));
        let grant = self
            .services
            .tokens
            .mint(&model.id, ttl, chrono::Utc::now());
        let host = match (self.services.lan_address)() {
            Ok(address) => Some(address.to_string()),
            Err(why) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "stream_token",
                    error = %why,
                    "stream grant has no LAN address; only a loopback client can use it"
                );
                None
            }
        };
        tracing::info!(
            target: TRACE_TARGET,
            op = "stream_token",
            model = %model.id,
            expires_at = %grant.expires_at,
            host = host.as_deref().unwrap_or(""),
            cert_sha256 = %endpoint.cert_sha256,
            "stream token minted"
        );
        respond_json(
            request,
            200,
            &serde_json::json!({
                "token": grant.token,
                "model": grant.model,
                "expiresAt": grant.expires_at.to_rfc3339(),
                "scheme": crate::stt_stream::server::STREAM_SCHEME,
                "host": host,
                "port": endpoint.port,
                "path": crate::stt_stream::server::STREAM_PATH,
                "certSha256": endpoint.cert_sha256,
                "certPem": endpoint.cert_pem,
            }),
        )
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

    /// `/daemon/*`: what the tray UI sees and does.
    fn handle_daemon(
        &self,
        mut request: Request,
        method: &Method,
        url: &str,
    ) -> std::io::Result<()> {
        let Some(control) = &self.control else {
            return respond_json(
                request,
                503,
                &serde_json::json!({ "error": "daemon_control_unavailable" }),
            );
        };
        let path = url.split('?').next().unwrap_or("/");
        match (method, path) {
            (Method::Get, "/daemon/status") => {
                let from_tray = request.headers().iter().any(|h| {
                    h.field.equiv(crate::daemon_client::CLIENT_HEADER)
                        && h.value.as_str() == crate::daemon_client::TRAY_CLIENT
                });
                if from_tray {
                    control.note_tray_poll(crate::tray_watchdog::now_ms());
                }
                let status = control.status(&self.observers, self.gate.is_busy());
                respond_serialised(request, 200, &status)
            }
            (Method::Get, "/daemon/logs") => {
                let after = query_param(url, "after")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let (entries, seq) = crate::runtime::recent_logs_after(&self.observers, after);
                respond_serialised(request, 200, &crate::daemon_api::LogsPage { entries, seq })
            }
            (Method::Post, "/daemon/pause") => {
                let paused = control.set_paused(true);
                respond_json(request, 200, &serde_json::json!({ "paused": paused }))
            }
            (Method::Post, "/daemon/resume") => {
                let paused = control.set_paused(false);
                respond_json(request, 200, &serde_json::json!({ "paused": paused }))
            }
            (Method::Get, "/daemon/config") => {
                respond_serialised(request, 200, &control.editable_config())
            }
            (Method::Put, "/daemon/config") => {
                let body = match read_body(&mut request)? {
                    BodyOutcome::Ok(body) => body,
                    BodyOutcome::TooLarge => return respond_too_large(request),
                };
                let edit: crate::daemon_api::EditableConfig = match serde_json::from_str(&body) {
                    Ok(edit) => edit,
                    Err(err) => {
                        return respond_error(request, 400, "bad_request", &err.to_string())
                    }
                };
                match control.update_config(edit) {
                    Ok(saved) => respond_serialised(request, 200, &saved),
                    Err(err @ crate::control::ControlError::Invalid(_)) => {
                        respond_error(request, 400, "invalid_config", &err.to_string())
                    }
                    Err(err) => respond_error(request, 500, "config_not_saved", &err.to_string()),
                }
            }
            (Method::Post, "/daemon/registration/reset") => {
                match control.request_registration_reset() {
                    Ok(()) => respond_json(request, 202, &serde_json::json!({ "ok": true })),
                    Err(err) => respond_error(request, 409, "not_rejected", &err.to_string()),
                }
            }
            (Method::Post, "/daemon/shutdown") => {
                control.shutdown();
                respond_json(request, 202, &serde_json::json!({ "ok": true }))
            }
            _ => respond_error(request, 404, "not_found", "no such daemon route"),
        }
    }

    fn handle_job_log(&self, request: Request, id: &str) -> std::io::Result<()> {
        match crate::job_log::global().get(id) {
            Some(log) => respond_serialised(request, 200, &log),
            None => respond_error(request, 404, "unknown_job", "no log captured for that job"),
        }
    }

    fn handle_job_thumbnail(&self, request: Request, id: &str) -> std::io::Result<()> {
        match self.observers.thumbnails.get(id) {
            Some(png) => respond(request, 200, "image/png", &png),
            None => respond_error(request, 404, "no_thumbnail", "no thumbnail for that job"),
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
    read_body_capped(request, MAX_BODY_BYTES)
}

/// [`read_body`] with its own cap.
fn read_body_capped(request: &mut Request, cap: usize) -> std::io::Result<BodyOutcome> {
    // Declared length first — reject without reading a byte.
    if matches!(request.body_length(), Some(len) if len > cap) {
        return Ok(BodyOutcome::TooLarge);
    }
    // Then a hard cap on the reader for chunked / lying senders: read
    // at most one byte past the cap so overflow is detectable.
    let mut body = String::new();
    use std::io::Read as _;
    request
        .as_reader()
        .take(cap as u64 + 1)
        .read_to_string(&mut body)?;
    if body.len() > cap {
        return Ok(BodyOutcome::TooLarge);
    }
    Ok(BodyOutcome::Ok(body))
}

/// The raw request body, or `None` past `cap` bytes.
fn read_bytes_capped(request: &mut Request, cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    if matches!(request.body_length(), Some(len) if len > cap) {
        return Ok(None);
    }
    let mut body = Vec::new();
    use std::io::Read as _;
    request
        .as_reader()
        .take(cap as u64 + 1)
        .read_to_end(&mut body)?;
    Ok((body.len() <= cap).then_some(body))
}

fn respond_too_large(request: Request) -> std::io::Result<()> {
    respond_too_large_capped(request, MAX_BODY_BYTES)
}

fn respond_too_large_capped(request: Request, cap: usize) -> std::io::Result<()> {
    respond(
        request,
        413,
        "text/plain",
        format!("request body exceeds {cap} bytes").as_bytes(),
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

/// Map a [`LocalError`] onto an HTTP response: catalog/contract errors
/// (unknown or wrong-kind model, none configured) are the caller's
/// fault → 400 with the message; an engine failure is 500.
/// A chat's pictures are inline, and the model it resolves to reads images.  No pictures: `Ok`
/// whatever the model.
fn chat_images_readable(
    catalog: &Catalog,
    model_id: Option<&str>,
    images: &[String],
) -> Result<(), LocalError> {
    if images.is_empty() {
        return Ok(());
    }
    if let Some(index) = images.iter().position(|u| !u.starts_with("data:image/")) {
        return Err(LocalError::BadInput(format!(
            "images[{index}] must be a data:image/...;base64, URL"
        )));
    }
    let model = crate::local::resolve_llm(catalog, model_id)?;
    let task = Task::Llm(LlmParams {
        images: images.to_vec(),
        ..Default::default()
    });
    crate::vision::check(&model.id, &task, Some(&model.source))
        .map_err(|refusal| LocalError::BadInput(refusal.to_string()))
}

fn respond_local_err(request: Request, err: LocalError) -> std::io::Result<()> {
    let status = match err {
        LocalError::Engine(_) => 500,
        LocalError::DeadlineExceeded => 504,
        LocalError::InsufficientMemory(_) => 409,
        _ => 400,
    };
    respond(request, status, "text/plain", err.to_string().as_bytes())
}

fn respond_llm(request: Request, outcome: Result<TaskResult, LocalError>) -> std::io::Result<()> {
    match outcome {
        Ok(TaskResult::Llm { json }) => match serde_json::to_vec(&json) {
            Ok(bytes) => respond(request, 200, "application/json", &bytes),
            Err(e) => respond(request, 500, "text/plain", e.to_string().as_bytes()),
        },
        Ok(_) => respond(request, 500, "text/plain", b"unexpected non-llm result"),
        Err(err) => respond_local_err(request, err),
    }
}

/// `/models/<id><suffix>` -> `Some(id)` for a non-empty id without slashes.
fn lifecycle_route<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    let id = path.strip_prefix("/models/")?.strip_suffix(suffix)?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// `/jobs/<id><suffix>` -> `Some(id)` for a non-empty id without slashes.
fn job_route<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    let id = path.strip_prefix("/jobs/")?.strip_suffix(suffix)?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// The value of query parameter `name` in `url`, if present.
fn query_param<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

/// The wire shape of a model's status.
fn status_json(status: &ModelStatus) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": status.id,
        "state": status.state.name(),
        "resident": status.resident,
        "since": status.since.to_rfc3339(),
    });
    if let ModelState::Failed { reason } = &status.state {
        body["error"] = reason.clone().into();
    }
    body
}

fn respond_serialised<T: serde::Serialize>(
    request: Request,
    status: u16,
    body: &T,
) -> std::io::Result<()> {
    match serde_json::to_vec(body) {
        Ok(bytes) => respond(request, status, "application/json", &bytes),
        Err(err) => respond(request, 500, "text/plain", err.to_string().as_bytes()),
    }
}

fn respond_error(request: Request, status: u16, code: &str, message: &str) -> std::io::Result<()> {
    respond_serialised(
        request,
        status,
        &crate::daemon_api::ErrorBody {
            error: code.to_string(),
            message: Some(message.to_string()),
        },
    )
}

fn respond_json(request: Request, status: u16, body: &serde_json::Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    respond(request, status, "application/json", &bytes)
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
                synthetic_model_of("mesh", TaskKind::Mesh),
            ],
            ..Default::default()
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
            exclusive_group: None,
        }
    }

    const TEST_TOKEN: &str = "test-token-0123456789abcdef";

    struct Harness {
        url: String,
        observers: WorkerObservers,
        host: crate::host::ModelHost,
        services: ModelServices,
        /// What the machine's LAN address is right now; a test changes it as a new DHCP
        /// lease would.
        lan: Arc<parking_lot::Mutex<Result<std::net::Ipv4Addr, NoLanAddress>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn start(catalog: Catalog) -> Self {
            Self::start_with_gate(catalog, JobGate::new())
        }

        fn start_with_gate(catalog: Catalog, gate: JobGate) -> Self {
            Self::start_full(catalog, gate, 20.0)
        }

        /// Start with a device reporting `free_gib` of free memory.
        fn start_with_free(catalog: Catalog, free_gib: f32) -> Self {
            Self::start_full(catalog, JobGate::new(), free_gib)
        }

        fn start_full(catalog: Catalog, gate: JobGate, free_gib: f32) -> Self {
            let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
            let observers = WorkerObservers::default();
            let catalog = Arc::new(Mutex::new(catalog));
            let host = crate::host::ModelHost::new(
                catalog.clone(),
                Arc::new(crate::test_support::InstantRuntime),
                Arc::new(crate::test_support::FixedProbe(free_gib)),
                crate::residency::Residency::load_for_serving(None),
            );
            let mut services = ModelServices::new(host.clone());
            *services.stream_endpoint.write() = Some(test_stream_endpoint());
            let lan = Arc::new(parking_lot::Mutex::new(Ok(std::net::Ipv4Addr::new(
                192, 168, 1, 133,
            ))));
            let reported = lan.clone();
            services.lan_address = Arc::new(move || reported.lock().clone());
            let api = LocalApi::bind(
                "127.0.0.1:0",
                engine,
                catalog,
                None,
                observers.clone(),
                TEST_TOKEN.to_string(),
                gate.clone(),
                None,
                services.clone(),
            )
            .unwrap();
            let url = api.url();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = std::thread::spawn(move || api.serve(&stop_thread));
            Harness {
                url,
                observers,
                host,
                services,
                lan,
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

    fn test_host(catalog: &Arc<Mutex<Catalog>>) -> crate::host::ModelHost {
        crate::host::ModelHost::new(
            catalog.clone(),
            Arc::new(crate::test_support::InstantRuntime),
            Arc::new(crate::test_support::FixedProbe(20.0)),
            crate::residency::Residency::load_for_serving(None),
        )
    }

    fn seeded_catalog() -> Catalog {
        Catalog {
            models: vec![synthetic_model("synthetic-img")],
            ..Default::default()
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

    fn json(res: reqwest::blocking::Response) -> (u16, serde_json::Value) {
        let status = res.status().as_u16();
        (status, res.json().unwrap())
    }

    fn wait_state(h: &Harness, id: &str, want: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (status, body) = json(h.get(&format!("/models/{id}/state")).send().unwrap());
            assert_eq!(status, 200, "{body}");
            if body["state"] == want {
                return body;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never reached {want}: {body}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn get_models_carries_state_and_residency() {
        let h = Harness::start(seeded_catalog());
        let (status, body) = json(h.get("/models").send().unwrap());
        assert_eq!(status, 200);
        let first = &body.as_array().unwrap()[0];
        assert_eq!(first["state"], "unloaded");
        assert_eq!(first["resident"], false);
        assert!(first["id"].is_string(), "catalogue fields stay: {first}");
    }

    #[test]
    fn load_reaches_loaded_and_marks_resident() {
        let h = Harness::start(seeded_catalog());
        let (status, body) = json(h.post("/models/synthetic-img/load").send().unwrap());
        assert!(status == 202 || status == 200, "{status} {body}");
        assert_eq!(body["id"], "synthetic-img");
        assert_eq!(body["resident"], true);
        let body = wait_state(&h, "synthetic-img", "loaded");
        assert!(body["since"].is_string());
        let (status, _) = json(h.post("/models/synthetic-img/load").send().unwrap());
        assert_eq!(status, 200, "loading a loaded model is a no-op");
    }

    #[test]
    fn unload_frees_and_clears_residency() {
        let h = Harness::start(seeded_catalog());
        h.post("/models/synthetic-img/load").send().unwrap();
        wait_state(&h, "synthetic-img", "loaded");
        let (status, body) = json(h.post("/models/synthetic-img/unload").send().unwrap());
        assert!(status == 202 || status == 200, "{status} {body}");
        assert_eq!(body["resident"], false);
        wait_state(&h, "synthetic-img", "unloaded");
        let (status, _) = json(h.post("/models/synthetic-img/unload").send().unwrap());
        assert_eq!(status, 200, "unloading an unloaded model is a no-op");
    }

    #[test]
    fn a_load_that_does_not_fit_is_a_409_with_the_numbers() {
        let mut catalog = seeded_catalog();
        catalog.models[0].vram_gb_estimate = 8.0;
        let h = Harness::start_with_free(catalog, 4.0);
        let (status, body) = json(h.post("/models/synthetic-img/load").send().unwrap());
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"], "insufficient_memory");
        assert_eq!(body["neededGib"], 8.0);
        assert_eq!(body["freeGib"], 4.0);
        assert!(body["marginGib"].is_number());
        wait_state(&h, "synthetic-img", "unloaded");
    }

    #[test]
    fn unknown_models_are_404_on_every_lifecycle_route() {
        let h = Harness::start(seeded_catalog());
        for res in [
            h.get("/models/nope/state").send().unwrap(),
            h.post("/models/nope/load").send().unwrap(),
            h.post("/models/nope/unload").send().unwrap(),
        ] {
            let (status, body) = json(res);
            assert_eq!(status, 404);
            assert_eq!(body["error"], "unknown_model");
        }
    }

    #[test]
    fn a_disabled_model_cannot_be_loaded() {
        let mut catalog = seeded_catalog();
        catalog.models[0].enabled = false;
        let h = Harness::start(catalog);
        let (status, body) = json(h.post("/models/synthetic-img/load").send().unwrap());
        assert_eq!(status, 400);
        assert_eq!(body["error"], "model_disabled");
    }

    #[test]
    fn lifecycle_routes_need_the_token() {
        let h = Harness::start(seeded_catalog());
        let res = reqwest::blocking::Client::new()
            .post(format!("{}/models/synthetic-img/load", h.url))
            .send()
            .unwrap();
        assert_eq!(res.status(), 401);
        wait_state(&h, "synthetic-img", "unloaded");
    }

    #[test]
    fn deleting_a_loaded_model_unloads_it_first() {
        let h = Harness::start(seeded_catalog());
        h.post("/models/synthetic-img/load").send().unwrap();
        wait_state(&h, "synthetic-img", "loaded");
        let res = reqwest::blocking::Client::new()
            .delete(format!("{}/models/synthetic-img", h.url))
            .bearer_auth(TEST_TOKEN)
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        let unloaded = h.host.wait_for(
            "synthetic-img",
            |s| *s == crate::lifecycle::ModelState::Unloaded,
            std::time::Duration::from_secs(5),
        );
        assert!(unloaded.is_some(), "weights freed after delete");
        assert_eq!(h.host.loaded_gib(), 0.0);
    }

    fn llm_catalog() -> Catalog {
        Catalog {
            models: vec![synthetic_model_of("chat-llm", TaskKind::Llm)],
            ..Default::default()
        }
    }

    fn chat(h: &Harness, body: serde_json::Value) -> (u16, serde_json::Value) {
        json(h.post("/v1/chat/completions").json(&body).send().unwrap())
    }

    #[test]
    fn chat_is_served_on_the_lane_of_a_loaded_model() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let (status, body) = chat(
            &h,
            serde_json::json!({ "model": "chat-llm", "messages": [{ "role": "user", "content": "hi" }] }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["choices"][0]["message"]["content"], "resident:hi");
    }

    #[test]
    fn a_chat_whose_deadline_has_passed_is_dropped_without_running() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "chat-llm",
                "deadline_ms": 0,
                "messages": [{ "role": "user", "content": "hi" }],
            }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 504);
        assert!(res.text().unwrap().contains("deadline"));
        let job = h
            .observers
            .local_jobs
            .lock()
            .front()
            .cloned()
            .expect("recorded");
        assert!(
            matches!(&job.outcome, JobOutcome::Failed { reason } if reason.contains("deadline")),
            "{:?}",
            job.outcome
        );
    }

    #[test]
    fn a_chat_within_its_deadline_is_answered() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let (status, body) = chat(
            &h,
            serde_json::json!({
                "model": "chat-llm",
                "deadline_ms": 60_000,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["choices"][0]["message"]["content"], "resident:hi");
    }

    #[test]
    fn a_stream_whose_deadline_has_passed_ends_on_an_error() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "chat-llm",
                "stream": true,
                "deadline_ms": 0,
                "messages": [{ "role": "user", "content": "hi" }],
            }))
            .send()
            .unwrap();
        let (frames, done) = sse(&res.text().unwrap());
        assert!(done);
        let last = frames.last().unwrap();
        assert!(
            last["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("deadline")),
            "{last}"
        );
    }

    #[test]
    fn chat_uses_the_default_llm_when_it_is_loaded() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let (_, body) = chat(
            &h,
            serde_json::json!({ "messages": [{ "role": "user", "content": "yo" }] }),
        );
        assert_eq!(body["choices"][0]["message"]["content"], "resident:yo");
    }

    /// The JSON payloads of an SSE body, and whether it ended with [DONE].
    fn sse(body: &str) -> (Vec<serde_json::Value>, bool) {
        let data: Vec<&str> = body
            .split("\n\n")
            .filter_map(|e| e.strip_prefix("data: "))
            .collect();
        let done = data.last() == Some(&"[DONE]");
        let frames = data
            .iter()
            .filter(|d| **d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        (frames, done)
    }

    #[test]
    fn a_loaded_model_streams_its_answer_as_server_sent_events() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "chat-llm",
                "stream": true,
                "messages": [{ "role": "user", "content": "hi there" }],
            }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "text/event-stream");
        let (frames, done) = sse(&res.text().unwrap());
        assert!(done, "ends with [DONE]");
        let text: String = frames
            .iter()
            .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "resident:hi there");
        let last = frames.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["total_tokens"], 5);
        assert!(frames.len() > 3, "more than one content chunk: {frames:?}");
        let jobs = h.observers.local_jobs.lock().clone();
        assert_eq!(jobs.front().unwrap().model, "chat-llm");
    }

    #[test]
    fn streaming_needs_the_model_loaded() {
        let h = Harness::start(llm_catalog());
        let (status, body) = chat(
            &h,
            serde_json::json!({ "stream": true, "messages": [{ "role": "user", "content": "x" }] }),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"], "model_not_loaded");
    }

    #[test]
    fn streaming_an_unknown_model_is_a_400() {
        let h = Harness::start(llm_catalog());
        let res = h
            .post("/v1/chat/completions")
            .json(&serde_json::json!({ "model": "nope", "stream": true, "messages": [] }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn tokenize_counts_with_the_loaded_model() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let (status, body) = json(
            h.post("/tokenize")
                .json(&serde_json::json!({ "content": "hello" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["tokens"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn tokenize_refuses_what_it_cannot_do() {
        let h = Harness::start(llm_catalog());
        let (status, body) = json(
            h.post("/tokenize")
                .json(&serde_json::json!({ "content": "x" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"], "model_not_loaded");
        let (status, body) = json(
            h.post("/tokenize")
                .json(&serde_json::json!({ "content": "x", "model": "nope" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 404, "{body}");
        let res = h.post("/tokenize").body("no").send().unwrap();
        assert_eq!(res.status(), 400);
        let res = reqwest::blocking::Client::new()
            .post(format!("{}/tokenize", h.url))
            .json(&serde_json::json!({ "content": "x" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 401);
    }

    #[test]
    fn chat_template_kwargs_reach_the_model() {
        let h = Harness::start(llm_catalog());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let (_, body) = chat(
            &h,
            serde_json::json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "chat_template_kwargs": { "enable_thinking": false },
            }),
        );
        assert_eq!(
            body["kwargs"],
            serde_json::json!({ "enable_thinking": false })
        );
    }

    /// A synthetic chat model with a vision projector: it reads images.
    fn vision_catalog() -> Catalog {
        let mut vision = synthetic_model_of("vision-llm", TaskKind::Llm);
        vision.source.files.push(crate::types::ModelFile {
            role: crate::types::ModelFileRole::TextEncoderVision,
            url: "https://models.invalid/mmproj.gguf".into(),
            filename: "mmproj.gguf".into(),
            approx_bytes: None,
            sha256: None,
        });
        Catalog {
            models: vec![vision, synthetic_model_of("chat-llm", TaskKind::Llm)],
            ..Default::default()
        }
    }

    fn inline_png(width: u32, height: u32) -> String {
        use base64::Engine as _;
        let image = image::RgbImage::new(width, height);
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(out.into_inner())
        )
    }

    fn chat_text(h: &Harness, body: serde_json::Value) -> (u16, String) {
        let response = h.post("/v1/chat/completions").json(&body).send().unwrap();
        (response.status().as_u16(), response.text().unwrap())
    }

    #[test]
    fn a_vision_model_reads_inline_images_as_a_transient_job() {
        let h = Harness::start(vision_catalog());
        let (status, body) = chat(
            &h,
            serde_json::json!({
                "model": "vision-llm",
                "messages": [{ "role": "user", "content": "what is it?" }],
                "images": [inline_png(2, 1)],
            }),
        );
        assert_eq!(status, 200, "{body}");
        let content = body["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(content.ends_with("(read 1 images: 2x1)"), "{content}");
    }

    #[test]
    fn a_loaded_vision_model_gets_the_images_and_schema_on_its_lane() {
        let h = Harness::start(vision_catalog());
        h.post("/models/vision-llm/load").send().unwrap();
        wait_state(&h, "vision-llm", "loaded");
        let schema = serde_json::json!({ "type": "object" });
        let (status, body) = chat(
            &h,
            serde_json::json!({
                "model": "vision-llm",
                "messages": [{ "role": "user", "content": "what is it?" }],
                "images": [inline_png(1, 1), inline_png(1, 1)],
                "response_format": { "type": "json_schema", "json_schema": { "name": "a", "schema": schema } },
            }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["images"], 2, "{body}");
        assert_eq!(body["jsonSchema"], schema, "{body}");
    }

    #[test]
    fn images_for_a_model_that_reads_none_are_refused() {
        let h = Harness::start(vision_catalog());
        let (status, body) = chat_text(
            &h,
            serde_json::json!({
                "model": "chat-llm",
                "messages": [{ "role": "user", "content": "what is it?" }],
                "images": [inline_png(1, 1)],
            }),
        );
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("reads no images"), "{body}");
        assert!(h.observers.local_jobs.lock().is_empty(), "nothing ran");
    }

    #[test]
    fn images_must_travel_inline() {
        let h = Harness::start(vision_catalog());
        let (status, body) = chat_text(
            &h,
            serde_json::json!({
                "model": "vision-llm",
                "messages": [{ "role": "user", "content": "what is it?" }],
                "images": ["https://example.invalid/a.png"],
            }),
        );
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("data:image"), "{body}");
    }

    #[test]
    fn a_response_format_other_than_a_json_schema_or_text_is_refused() {
        let h = Harness::start(llm_catalog());
        let (status, body) = chat_text(
            &h,
            serde_json::json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "response_format": { "type": "json_object" },
            }),
        );
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("response_format"), "{body}");
    }

    #[test]
    fn response_format_maps_to_the_schema_it_names() {
        let schema = serde_json::json!({ "type": "object" });
        let named = |v: serde_json::Value| schema_of(Some(v)).map_err(|e| e.to_string());
        assert_eq!(schema_of(None).unwrap(), None);
        assert_eq!(named(serde_json::json!({ "type": "text" })), Ok(None));
        assert_eq!(
            named(
                serde_json::json!({ "type": "json_schema", "json_schema": { "schema": schema } })
            ),
            Ok(Some(schema))
        );
        let missing = named(serde_json::json!({ "type": "json_schema", "json_schema": {} }));
        assert!(missing.unwrap_err().contains("json_schema.schema"));
        let untyped = named(serde_json::json!({}));
        assert!(untyped.unwrap_err().contains("(missing)"));
    }

    #[test]
    fn a_chat_with_pictures_may_be_larger_than_a_plain_request() {
        let h = Harness::start(llm_catalog());
        let (status, body) = chat_text(
            &h,
            serde_json::json!({
                "messages": [{ "role": "user", "content": "x".repeat(MAX_BODY_BYTES + 1) }],
                "max_tokens": 1,
            }),
        );
        assert_eq!(status, 200, "{}", &body[..body.len().min(200)]);
    }

    #[test]
    fn the_model_list_says_which_models_read_images() {
        let h = Harness::start(vision_catalog());
        let (_, listed) = json(h.get("/models").send().unwrap());
        let reads = |id: &str| {
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id)
                .unwrap()["images"]
                .clone()
        };
        assert_eq!(reads("vision-llm"), true);
        assert_eq!(reads("chat-llm"), false);
    }

    #[test]
    fn chat_on_an_unloaded_model_runs_as_a_transient_job() {
        let h = Harness::start(llm_catalog());
        let (status, body) = chat(
            &h,
            serde_json::json!({ "model": "chat-llm", "messages": [{ "role": "user", "content": "hi" }] }),
        );
        assert_eq!(status, 200, "{body}");
        let content = body["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(!content.starts_with("resident:"), "{content}");
    }

    #[test]
    fn a_resident_chat_is_recorded_as_a_local_job_and_skips_the_job_gate() {
        let gate = JobGate::new();
        let h = Harness::start_with_gate(llm_catalog(), gate.clone());
        h.post("/models/chat-llm/load").send().unwrap();
        wait_state(&h, "chat-llm", "loaded");
        let _held = gate.try_reserve().expect("a transient job holds the gate");
        let (status, _) = chat(
            &h,
            serde_json::json!({ "model": "chat-llm", "messages": [{ "role": "user", "content": "lane" }] }),
        );
        assert_eq!(status, 200, "a loaded model serves on its own lane");
        let jobs = h.observers.local_jobs.lock().clone();
        let last = jobs.front().expect("recorded");
        assert_eq!(last.model, "chat-llm");
        assert_eq!(last.prompt, "lane");
    }

    fn test_stream_endpoint() -> crate::stt_stream::server::StreamEndpoint {
        crate::stt_stream::server::StreamEndpoint {
            port: 4798,
            cert_sha256: "ab".repeat(32),
            cert_pem: "-----BEGIN CERTIFICATE-----\nTEST\n-----END CERTIFICATE-----\n".into(),
        }
    }

    fn stream_catalog() -> Catalog {
        let mut stt = synthetic_model_of("stt-a", TaskKind::AudioStt);
        stt.source.engine = crate::types::ModelEngine::Parakeet;
        Catalog {
            models: vec![stt, synthetic_model_of("chat-llm", TaskKind::Llm)],
            ..Default::default()
        }
    }

    #[test]
    fn stream_tokens_are_minted_for_streaming_models() {
        let h = Harness::start(stream_catalog());
        let (status, body) = json(
            h.post("/stream-tokens")
                .json(&serde_json::json!({ "model": "stt-a", "ttlSecs": 600 }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 200, "{body}");
        let token = body["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(body["model"], "stt-a");
        assert_eq!(body["scheme"], "wss");
        assert_eq!(body["host"], "192.168.1.133");
        assert_eq!(body["port"], 4798);
        assert_eq!(body["path"], "/transcribe");
        assert_eq!(body["certSha256"], "ab".repeat(32));
        assert_eq!(body["certPem"], test_stream_endpoint().cert_pem);
        assert!(body["expiresAt"].is_string());
        assert_eq!(
            h.services.tokens.check(token, chrono::Utc::now()),
            Ok("stt-a".to_string()),
            "the listener accepts it"
        );
    }

    #[test]
    fn every_grant_names_the_lan_address_of_its_moment() {
        let h = Harness::start(stream_catalog());
        let mint = || {
            json(
                h.post("/stream-tokens")
                    .json(&serde_json::json!({ "model": "stt-a" }))
                    .send()
                    .unwrap(),
            )
            .1
        };
        assert_eq!(mint()["host"], "192.168.1.133");
        *h.lan.lock() = Ok(std::net::Ipv4Addr::new(192, 168, 1, 77));
        assert_eq!(
            mint()["host"],
            "192.168.1.77",
            "a new lease reaches the next grant"
        );
    }

    #[test]
    fn a_grant_without_a_lan_address_says_so_and_still_holds() {
        let h = Harness::start(stream_catalog());
        *h.lan.lock() = Err(NoLanAddress::NotPrivate("100.64.0.7".parse().unwrap()));
        let (status, body) = json(
            h.post("/stream-tokens")
                .json(&serde_json::json!({ "model": "stt-a" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 200, "{body}");
        assert!(body["host"].is_null());
        assert_eq!(body["token"].as_str().unwrap().len(), 64);
        assert_eq!(body["port"], 4798);
    }

    #[test]
    fn stream_tokens_are_refused_for_other_models() {
        let h = Harness::start(stream_catalog());
        let (status, body) = json(
            h.post("/stream-tokens")
                .json(&serde_json::json!({ "model": "chat-llm" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 400);
        assert_eq!(body["error"], "not_a_stream_model");
        let (status, body) = json(
            h.post("/stream-tokens")
                .json(&serde_json::json!({ "model": "nope" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 404);
        assert_eq!(body["error"], "unknown_model");
    }

    #[test]
    fn stream_tokens_need_a_running_listener() {
        let mut h = Harness::start(stream_catalog());
        *h.services.stream_endpoint.write() = None;
        let (status, body) = json(
            h.post("/stream-tokens")
                .json(&serde_json::json!({ "model": "stt-a" }))
                .send()
                .unwrap(),
        );
        assert_eq!(status, 503);
        assert_eq!(body["error"], "stream_listener_down");
        let _ = &mut h;
    }

    #[test]
    fn stream_tokens_need_the_install_token() {
        let h = Harness::start(stream_catalog());
        let res = reqwest::blocking::Client::new()
            .post(format!("{}/stream-tokens", h.url))
            .json(&serde_json::json!({ "model": "stt-a" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 401);
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

    fn tiny_png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(3, 3, image::Rgb([40, 80, 120]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn mesh_answers_a_glb_for_a_raw_picture() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/mesh?seed=3")
            .header("content-type", "image/png")
            .body(tiny_png())
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "model/gltf-binary");
        let glb = res.bytes().unwrap();
        assert_eq!(crate::glb::check(&glb), Ok(()));
        assert_eq!(
            glb.as_ref(),
            crate::glb::textured_cube([40, 80, 120]).as_slice()
        );
    }

    #[test]
    fn mesh_reads_a_json_picture_and_names_its_model() {
        use base64::Engine as _;
        let h = Harness::start(multi_kind_catalog());
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(tiny_png())
        );
        let res = h
            .post("/mesh")
            .json(&serde_json::json!({ "image": image, "model": "mesh" }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
        let unknown = h
            .post("/mesh")
            .json(&serde_json::json!({ "image": image, "model": "nope" }))
            .send()
            .unwrap();
        assert_eq!(unknown.status(), 400);
    }

    #[test]
    fn mesh_refuses_what_is_not_a_picture_and_unknown_options() {
        let h = Harness::start(multi_kind_catalog());
        let res = h
            .post("/mesh")
            .header("content-type", "image/png")
            .body(b"plain words".to_vec())
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
        let res = h
            .post("/mesh?steps=9")
            .header("content-type", "image/png")
            .body(tiny_png())
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
        assert!(res.text().unwrap().contains("steps"));
    }

    #[test]
    fn mesh_respects_the_busy_gate() {
        let gate = JobGate::new();
        let h = Harness::start_with_gate(multi_kind_catalog(), gate.clone());
        let _held = gate.try_reserve().unwrap();
        let res = h
            .post("/mesh")
            .header("content-type", "image/png")
            .body(tiny_png())
            .send()
            .unwrap();
        assert_eq!(res.status(), 503);
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
    fn daemon_routes_need_daemon_control() {
        let h = Harness::start(multi_kind_catalog());
        let resp = h.get("/daemon/status").send().unwrap();
        assert_eq!(resp.status(), 503);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["error"], "daemon_control_unavailable");
    }

    #[test]
    fn daemon_routes_need_the_token() {
        let h = Harness::start(multi_kind_catalog());
        let resp = reqwest::blocking::Client::new()
            .get(format!("{}/daemon/status", h.url))
            .send()
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[test]
    fn a_status_poll_marked_as_the_tray_s_is_noted_for_the_watchdog() {
        let daemon = crate::test_support::DaemonHarness::start();
        let client = reqwest::blocking::Client::new();
        let poll = |marked: bool| {
            let mut request = client
                .get(format!("{}/daemon/status", daemon.url))
                .bearer_auth(crate::test_support::HARNESS_TOKEN);
            if marked {
                request = request.header(
                    crate::daemon_client::CLIENT_HEADER,
                    crate::daemon_client::TRAY_CLIENT,
                );
            }
            assert_eq!(request.send().unwrap().status(), 200);
        };
        poll(false);
        assert_eq!(daemon.control.tray_last_seen(), None);
        poll(true);
        assert!(daemon.control.tray_last_seen().is_some());
    }

    #[test]
    fn an_unknown_daemon_route_is_not_found() {
        let daemon = crate::test_support::DaemonHarness::start();
        let resp = reqwest::blocking::Client::new()
            .get(format!("{}/daemon/nope", daemon.url))
            .bearer_auth(crate::test_support::HARNESS_TOKEN)
            .send()
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn a_malformed_config_body_is_a_bad_request() {
        let daemon = crate::test_support::DaemonHarness::start();
        let resp = reqwest::blocking::Client::new()
            .put(format!("{}/daemon/config", daemon.url))
            .bearer_auth(crate::test_support::HARNESS_TOKEN)
            .body("{")
            .send()
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[test]
    fn the_models_listing_carries_since_and_the_failure() {
        let daemon = crate::test_support::DaemonHarness::start();
        let models = daemon.client().models().unwrap();
        assert!(models.iter().all(|m| m.since.is_some() && m.loadable));
        assert!(models.iter().all(|m| m.error.is_none()));
    }

    #[test]
    fn job_routes_and_query_params_parse() {
        assert_eq!(job_route("/jobs/local-1/log", "/log"), Some("local-1"));
        assert_eq!(job_route("/jobs//log", "/log"), None);
        assert_eq!(job_route("/jobs/a/b/log", "/log"), None);
        assert_eq!(query_param("/daemon/logs?after=12", "after"), Some("12"));
        assert_eq!(query_param("/daemon/logs?x=1&after=3", "after"), Some("3"));
        assert_eq!(query_param("/daemon/logs?afterx=3", "after"), None);
        assert_eq!(query_param("/daemon/logs", "after"), None);
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
        let catalog = Arc::new(Mutex::new(seeded_catalog()));
        let gate = JobGate::new();
        let api = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            catalog.clone(),
            None,
            observers,
            TEST_TOKEN.to_string(),
            gate.clone(),
            None,
            ModelServices::new(test_host(&catalog)),
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

        // Wait until the generation holds the job gate (a fixed sleep raced it on a loaded CI
        // runner), then time a /healthz: it must answer well within the generation's 400 ms.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gate.is_busy() {
            assert!(
                std::time::Instant::now() < deadline,
                "the generation never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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
        let res = h.post("/tts").body(big).send().unwrap();
        assert_eq!(res.status(), 413);
    }

    #[test]
    fn an_image_body_has_its_own_larger_cap() {
        let h = Harness::start(seeded_catalog());
        let big = "x".repeat(MAX_IMAGE_BODY_BYTES + 1);
        let res = h.post("/image").body(big).send().unwrap();
        assert_eq!(res.status(), 413);
    }

    #[test]
    fn an_edit_with_inline_images_past_the_general_cap_runs() {
        let h = Harness::start(seeded_catalog());
        // A base well past the general 1 MiB cap, as a local edit sends it.
        let base = format!("data:image/png;base64,{}", "A".repeat(2 * MAX_BODY_BYTES));
        let res = h
            .post("/image")
            .json(&serde_json::json!({ "prompt": "remove the mug", "refImageUrl": base }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn an_input_image_by_address_is_refused() {
        let h = Harness::start(seeded_catalog());
        let res = h
            .post("/image")
            .json(&serde_json::json!({
                "prompt": "remove the mug",
                "refImageUrl": "https://example.com/base.png"
            }))
            .send()
            .unwrap();
        assert_eq!(res.status(), 400);
        assert!(res
            .text()
            .unwrap()
            .contains("refImageUrl must be a data:image"));
    }

    #[test]
    fn body_at_the_cap_is_still_read() {
        // Boundary: exactly MAX_BODY_BYTES must not be rejected as too
        // large (it fails later as bad JSON, which is the point — the
        // size gate stayed out of the way).
        let h = Harness::start(seeded_catalog());
        let exact = "x".repeat(MAX_BODY_BYTES);
        let res = h.post("/tts").body(exact).send().unwrap();
        assert_eq!(res.status(), 400);
    }

    #[test]
    fn bind_refuses_an_empty_token() {
        let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
        let catalog = Arc::new(Mutex::new(seeded_catalog()));
        let err = LocalApi::bind(
            "127.0.0.1:0",
            engine,
            catalog.clone(),
            None,
            WorkerObservers::default(),
            String::new(),
            JobGate::new(),
            None,
            ModelServices::new(test_host(&catalog)),
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
