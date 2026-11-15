//! The warm image server: an `sd-server` child that keeps one image model loaded between transient
//! jobs, so the next picture of the same model skips the load.
//!
//! Rules: docs/engines/sdcpp.md#warm-server and docs/runtime/model-lifecycle.md#warm-models.
//! [`WarmImageServers`] holds at most one server.  It reaches the process and the HTTP API through
//! two seams, [`ServerLauncher`] and [`ServerClient`], so the state machine is tested with fakes;
//! the real ones ([`ProcessLauncher`], [`HttpClient`]) need an `sd-server` binary and are excluded
//! from coverage like `SdCppEngine::dispatch_image`, except the HTTP client, which is tested
//! against `wiremock`.
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const TRACE_TARGET: &str = "studio_worker::engine::sd_server";

/// How long a warm server may take to answer `GET /v1/models` after it starts: a cold load of
/// the largest image model from disk; 60 to 600.
pub const WARM_START_TIMEOUT: Duration = Duration::from_secs(180);
/// How long one picture may take on the warm server: the slowest model at its largest size; 60
/// to 900.
pub const WARM_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How often readiness is asked while a warm server starts; 100 to 1000 ms.
const READY_POLL: Duration = Duration::from_millis(250);
/// How often an idle warm server is checked for expiry; 1 to 30 s.
const IDLE_CHECK: Duration = Duration::from_secs(5);
/// Stderr lines kept for the error of a server that dies while starting.
const STDERR_TAIL_LINES: usize = 20;

/// What must be equal to reuse a warm server: the model id, its files and its load-time flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmKey {
    pub model: String,
    pub load_args: Vec<OsString>,
}

/// One picture's request, with every value resolved as `sd-cli` would get it.
#[derive(Debug, Clone, PartialEq)]
pub struct WarmRequest {
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f32,
    pub sampling_method: Option<String>,
    pub seed: Option<u64>,
    /// `png`, `jpeg` or `webp`.
    pub output_format: String,
}

impl WarmRequest {
    /// The `POST /v1/images/generations` body: the OpenAI fields, and the per-job values in
    /// `<sd_cpp_extra_args>` inside the prompt.
    pub fn body(&self) -> Value {
        let mut sample_params = json!({
            "sample_steps": self.steps,
            "guidance": { "txt_cfg": self.cfg_scale },
        });
        if let Some(method) = &self.sampling_method {
            sample_params["sample_method"] = json!(method);
        }
        let mut extra = json!({
            "width": self.width,
            "height": self.height,
            "seed": self.seed.map_or(json!(-1), |seed| json!(seed)),
            "sample_params": sample_params,
        });
        if let Some(negative) = self.negative_prompt.as_deref().filter(|n| !n.is_empty()) {
            extra["negative_prompt"] = json!(negative);
        }
        json!({
            "prompt": format!("{} <sd_cpp_extra_args>{}</sd_cpp_extra_args>", self.prompt, extra),
            "n": 1,
            "size": format!("{}x{}", self.width, self.height),
            "output_format": self.output_format,
            "output_compression": 90,
        })
    }
}

/// The picture in a `/v1/images/generations` answer: `data[0].b64_json`, decoded.
pub fn picture_of(answer: &Value) -> Result<Vec<u8>> {
    let encoded = answer
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("sd-server answered without data[0].b64_json"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("sd-server answered a picture that is not base64")?;
    if bytes.is_empty() {
        bail!("sd-server answered an empty picture");
    }
    Ok(bytes)
}

/// A running `sd-server` child.
pub trait ServerProcess: Send {
    /// `None` while it runs; how it ended once it has.
    fn exited(&mut self) -> Option<String>;
    /// Its last stderr lines, for an error.
    fn stderr_tail(&self) -> String;
    /// Stop it and wait for it.
    fn kill(&mut self);
}

/// Starts `sd-server` children.
pub trait ServerLauncher: Send + Sync {
    /// A loopback port nothing listens on now.
    fn free_port(&self) -> Result<u16>;
    fn spawn(&self, sd_server: &Path, args: &[OsString]) -> Result<Box<dyn ServerProcess>>;
}

/// Speaks to a warm server's HTTP API.
pub trait ServerClient: Send + Sync {
    /// Whether `GET /v1/models` answers `200`.
    fn ready(&self, port: u16) -> bool;
    /// One picture: `POST /v1/images/generations`, decoded.
    fn generate(&self, port: u16, body: &Value, timeout: Duration) -> Result<Vec<u8>>;
}

/// The time limits a [`WarmImageServers`] works to; tests shrink them.
#[derive(Debug, Clone, Copy)]
pub struct WarmTimings {
    pub start_timeout: Duration,
    pub ready_poll: Duration,
    pub request_timeout: Duration,
}

impl Default for WarmTimings {
    fn default() -> Self {
        Self {
            start_timeout: WARM_START_TIMEOUT,
            ready_poll: READY_POLL,
            request_timeout: WARM_REQUEST_TIMEOUT,
        }
    }
}

struct Running {
    key: WarmKey,
    port: u16,
    process: Box<dyn ServerProcess>,
    started: Instant,
    last_used: Instant,
    keep_warm: Duration,
}

/// The worker's warm image server, at most one (docs/engines/sdcpp.md#warm-server).
pub struct WarmImageServers {
    running: Mutex<Option<Running>>,
    launcher: Box<dyn ServerLauncher>,
    client: Box<dyn ServerClient>,
    timings: WarmTimings,
}

impl WarmImageServers {
    pub fn new(
        launcher: Box<dyn ServerLauncher>,
        client: Box<dyn ServerClient>,
        timings: WarmTimings,
    ) -> Self {
        Self {
            running: Mutex::new(None),
            launcher,
            client,
            timings,
        }
    }

    /// Make one picture on the warm server for `key`, starting (or replacing) it first when
    /// needed.  On any failure the server is stopped and the error returned: the caller runs the
    /// job on `sd-cli` instead.
    pub fn generate(
        &self,
        sd_server: &Path,
        key: WarmKey,
        start_args: &[OsString],
        keep_warm: Duration,
        request: &WarmRequest,
    ) -> Result<Vec<u8>> {
        let mut running = self.running.lock();
        if let Some(current) = running.as_mut() {
            if let Some(how) = current.process.exited() {
                warn!(
                    target: TRACE_TARGET,
                    op = "evict",
                    model = %current.key.model,
                    reason = "exited",
                    exit = %how,
                    held_s = current.started.elapsed().as_secs(),
                    "warm server ended on its own"
                );
                *running = None;
            }
        }
        if running.as_ref().is_some_and(|current| current.key != key) {
            stop(&mut running, "replaced");
        }
        if running.is_none() {
            *running = Some(self.start(sd_server, &key, start_args, keep_warm)?);
        } else {
            info!(target: TRACE_TARGET, op = "reuse", model = %key.model, "warm server reused");
        }
        let current = running.as_mut().expect("a warm server runs");
        current.keep_warm = keep_warm;
        let started = Instant::now();
        match self
            .client
            .generate(current.port, &request.body(), self.timings.request_timeout)
        {
            Ok(bytes) => {
                current.last_used = Instant::now();
                info!(
                    target: TRACE_TARGET,
                    op = "generate",
                    model = %key.model,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    bytes = bytes.len(),
                    "warm picture made"
                );
                Ok(bytes)
            }
            Err(error) => {
                stop(&mut running, "failed");
                Err(error.context("warm server generate"))
            }
        }
    }

    /// Whether `model` is the warm model now.
    pub fn holds(&self, model: &str) -> bool {
        self.running
            .lock()
            .as_ref()
            .is_some_and(|current| current.key.model == model)
    }

    /// Stop the warm server, if one runs.  Whether one was stopped.
    pub fn evict(&self, reason: &str) -> bool {
        let mut running = self.running.lock();
        let had = running.is_some();
        stop(&mut running, reason);
        had
    }

    /// Stop the warm server once `keepWarmSeconds` have passed since its last job.  Whether it
    /// was stopped.
    pub fn expire_idle(&self, now: Instant) -> bool {
        let mut running = self.running.lock();
        let Some(current) = running.as_mut() else {
            return false;
        };
        let idle = now.saturating_duration_since(current.last_used);
        if idle < current.keep_warm {
            return false;
        }
        info!(
            target: TRACE_TARGET,
            op = "expire",
            model = %current.key.model,
            idle_s = idle.as_secs(),
            "warm server expired"
        );
        current.process.kill();
        *running = None;
        true
    }

    fn start(
        &self,
        sd_server: &Path,
        key: &WarmKey,
        start_args: &[OsString],
        keep_warm: Duration,
    ) -> Result<Running> {
        match self.start_once(sd_server, key, start_args, keep_warm) {
            Ok(running) => Ok(running),
            Err(first) => {
                warn!(
                    target: TRACE_TARGET,
                    op = "start",
                    model = %key.model,
                    error = %first,
                    "warm server did not start; retrying once on another port"
                );
                self.start_once(sd_server, key, start_args, keep_warm)
            }
        }
    }

    fn start_once(
        &self,
        sd_server: &Path,
        key: &WarmKey,
        start_args: &[OsString],
        keep_warm: Duration,
    ) -> Result<Running> {
        let started = Instant::now();
        let port = self.launcher.free_port()?;
        let mut args: Vec<OsString> = vec![
            "--listen-ip".into(),
            "127.0.0.1".into(),
            "--listen-port".into(),
            port.to_string().into(),
        ];
        args.extend(start_args.iter().cloned());
        let mut process = self.launcher.spawn(sd_server, &args)?;
        loop {
            if self.client.ready(port) {
                break;
            }
            if let Some(how) = process.exited() {
                bail!(
                    "sd-server exited while starting ({how}): {}",
                    process.stderr_tail()
                );
            }
            if started.elapsed() >= self.timings.start_timeout {
                process.kill();
                bail!(
                    "sd-server was not ready within {} s: {}",
                    self.timings.start_timeout.as_secs(),
                    process.stderr_tail()
                );
            }
            std::thread::sleep(self.timings.ready_poll);
        }
        info!(
            target: TRACE_TARGET,
            op = "start",
            model = %key.model,
            port,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "warm server ready"
        );
        let now = Instant::now();
        Ok(Running {
            key: key.clone(),
            port,
            process,
            started: now,
            last_used: now,
            keep_warm,
        })
    }
}

impl Drop for WarmImageServers {
    fn drop(&mut self) {
        stop(&mut self.running.lock(), "dropped");
    }
}

fn stop(running: &mut Option<Running>, reason: &str) {
    if let Some(mut current) = running.take() {
        info!(
            target: TRACE_TARGET,
            op = "evict",
            model = %current.key.model,
            reason,
            held_s = current.started.elapsed().as_secs(),
            "warm server stopped"
        );
        current.process.kill();
    }
}

static WARM: OnceLock<Arc<WarmImageServers>> = OnceLock::new();

/// The process-wide warm server, started on first use with its idle-expiry thread.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn warm_servers() -> &'static Arc<WarmImageServers> {
    WARM.get_or_init(|| {
        let servers = Arc::new(WarmImageServers::new(
            Box::new(ProcessLauncher::new()),
            Box::new(HttpClient::new()),
            WarmTimings::default(),
        ));
        let watched = Arc::downgrade(&servers);
        std::thread::Builder::new()
            .name("warm-image-expiry".into())
            .spawn(move || loop {
                std::thread::sleep(IDLE_CHECK);
                match watched.upgrade() {
                    Some(servers) => {
                        servers.expire_idle(Instant::now());
                    }
                    None => break,
                }
            })
            .expect("spawning the warm image expiry thread");
        servers
    })
}

/// What admission needs of the warm server (docs/runtime/model-lifecycle.md#warm-models).
pub trait WarmHold: Send + Sync {
    /// Whether `model` is the warm model now.
    fn holds(&self, model: &str) -> bool;
    /// Stop the warm server, if one runs.  Whether one was stopped.
    fn evict(&self, reason: &str) -> bool;
}

impl WarmHold for WarmImageServers {
    fn holds(&self, model: &str) -> bool {
        WarmImageServers::holds(self, model)
    }
    fn evict(&self, reason: &str) -> bool {
        WarmImageServers::evict(self, reason)
    }
}

/// The process-wide warm server as admission sees it: nothing until one was ever started.
pub struct StartedWarm;

impl WarmHold for StartedWarm {
    fn holds(&self, model: &str) -> bool {
        started().is_some_and(|servers| servers.holds(model))
    }
    fn evict(&self, reason: &str) -> bool {
        evict_started(reason)
    }
}

/// The warm server, only if one was ever asked for: what admission, pause and shutdown consult.
pub fn started() -> Option<&'static Arc<WarmImageServers>> {
    WARM.get()
}

/// Stop the warm server, if one was ever started (a pause, active presence, shutdown).
pub fn evict_started(reason: &str) -> bool {
    started().is_some_and(|servers| servers.evict(reason))
}

/// `sd-server` beside the resolved `sd-cli`, when it is there.
pub fn sibling_sd_server(sd_cli: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "sd-server.exe"
    } else {
        "sd-server"
    };
    let candidate = sd_cli.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// The HTTP client of a warm server on loopback.
pub struct HttpClient {
    http: reqwest::blocking::Client,
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("building the warm server's HTTP client"),
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerClient for HttpClient {
    fn ready(&self, port: u16) -> bool {
        self.http
            .get(format!("http://127.0.0.1:{port}/v1/models"))
            .timeout(Duration::from_secs(2))
            .send()
            .is_ok_and(|response| response.status().is_success())
    }

    fn generate(&self, port: u16, body: &Value, timeout: Duration) -> Result<Vec<u8>> {
        let response = self
            .http
            .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
            .timeout(timeout)
            .json(body)
            .send()
            .context("sending the picture request to sd-server")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!(
                "sd-server answered {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let answer: Value = response.json().context("sd-server answered no JSON")?;
        picture_of(&answer)
    }
}

/// Starts `sd-server` from one long-lived thread: on Linux the child gets `PR_SET_PDEATHSIG`,
/// which fires when the thread that spawned it ends, so that thread must live as long as the
/// daemon.
pub struct ProcessLauncher {
    requests: Mutex<mpsc::Sender<SpawnRequest>>,
}

struct SpawnRequest {
    program: PathBuf,
    args: Vec<OsString>,
    reply: mpsc::Sender<std::io::Result<std::process::Child>>,
}

impl ProcessLauncher {
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn new() -> Self {
        let (requests, received) = mpsc::channel::<SpawnRequest>();
        std::thread::Builder::new()
            .name("warm-image-spawner".into())
            .spawn(move || {
                for request in received {
                    let _ = request
                        .reply
                        .send(spawn_child(&request.program, &request.args));
                }
            })
            .expect("spawning the warm image spawner thread");
        Self {
            requests: Mutex::new(requests),
        }
    }
}

impl Default for ProcessLauncher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn spawn_child(program: &Path, args: &[OsString]) -> std::io::Result<std::process::Child> {
    use std::process::Stdio;
    let mut command = crate::process::command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    crate::engine::sdcpp::apply_library_path(&mut command, program);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `prctl` is async-signal-safe and touches only the child being started.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn()
}

impl ServerLauncher for ProcessLauncher {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn free_port(&self) -> Result<u16> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("finding a free loopback port for sd-server")?;
        Ok(listener.local_addr()?.port())
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn spawn(&self, sd_server: &Path, args: &[OsString]) -> Result<Box<dyn ServerProcess>> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .lock()
            .send(SpawnRequest {
                program: sd_server.to_path_buf(),
                args: args.to_vec(),
                reply,
            })
            .map_err(|_| anyhow!("the warm image spawner thread is gone"))?;
        let mut child = answer
            .recv()
            .map_err(|_| anyhow!("the warm image spawner thread did not answer"))?
            .with_context(|| format!("starting {}", sd_server.display()))?;
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&tail);
            std::thread::Builder::new()
                .name("warm-image-stderr".into())
                .spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(stderr)
                        .lines()
                        .map_while(Result::ok)
                    {
                        debug!(target: TRACE_TARGET, op = "server", line = %line, "sd-server");
                        let mut tail = tail.lock();
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
                .context("spawning the sd-server stderr reader")?;
        }
        Ok(Box::new(ChildServer { child, tail }))
    }
}

struct ChildServer {
    child: std::process::Child,
    tail: Arc<Mutex<VecDeque<String>>>,
}

impl ServerProcess for ChildServer {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(error) => Some(format!("unknown ({error})")),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn stderr_tail(&self) -> String {
        let tail = self.tail.lock();
        if tail.is_empty() {
            "(no stderr)".into()
        } else {
            tail.iter().cloned().collect::<Vec<_>>().join(" | ")
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildServer {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Script {
        spawned: AtomicUsize,
        killed: AtomicUsize,
        spawn_args: Mutex<Vec<Vec<OsString>>>,
        exits_while_starting: AtomicBool,
        never_ready: AtomicBool,
        fail_generate: AtomicBool,
        generated: AtomicUsize,
        next_port: AtomicU16,
    }

    struct FakeProcess {
        script: Arc<Script>,
        exits: bool,
    }

    impl ServerProcess for FakeProcess {
        fn exited(&mut self) -> Option<String> {
            self.exits.then(|| "exit status: 1".to_string())
        }
        fn stderr_tail(&self) -> String {
            "error: model file not found".into()
        }
        fn kill(&mut self) {
            self.script.killed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeLauncher(Arc<Script>);

    impl ServerLauncher for FakeLauncher {
        fn free_port(&self) -> Result<u16> {
            Ok(40000 + self.0.next_port.fetch_add(1, Ordering::SeqCst))
        }
        fn spawn(&self, _sd_server: &Path, args: &[OsString]) -> Result<Box<dyn ServerProcess>> {
            self.0.spawned.fetch_add(1, Ordering::SeqCst);
            self.0.spawn_args.lock().push(args.to_vec());
            Ok(Box::new(FakeProcess {
                script: Arc::clone(&self.0),
                exits: self.0.exits_while_starting.load(Ordering::SeqCst),
            }))
        }
    }

    struct FakeClient(Arc<Script>);

    impl ServerClient for FakeClient {
        fn ready(&self, _port: u16) -> bool {
            !self.0.never_ready.load(Ordering::SeqCst)
                && !self.0.exits_while_starting.load(Ordering::SeqCst)
        }
        fn generate(&self, _port: u16, _body: &Value, _timeout: Duration) -> Result<Vec<u8>> {
            if self.0.fail_generate.load(Ordering::SeqCst) {
                bail!("connection reset");
            }
            self.0.generated.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1, 2, 3])
        }
    }

    fn servers() -> (WarmImageServers, Arc<Script>) {
        let script = Arc::new(Script::default());
        let servers = WarmImageServers::new(
            Box::new(FakeLauncher(Arc::clone(&script))),
            Box::new(FakeClient(Arc::clone(&script))),
            WarmTimings {
                start_timeout: Duration::from_millis(30),
                ready_poll: Duration::from_millis(5),
                request_timeout: Duration::from_secs(1),
            },
        );
        (servers, script)
    }

    fn key(model: &str) -> WarmKey {
        WarmKey {
            model: model.into(),
            load_args: vec![
                "--diffusion-model".into(),
                format!("/m/{model}.gguf").into(),
            ],
        }
    }

    fn request() -> WarmRequest {
        WarmRequest {
            prompt: "a lighthouse".into(),
            negative_prompt: None,
            width: 512,
            height: 512,
            steps: 4,
            cfg_scale: 1.0,
            sampling_method: Some("euler".into()),
            seed: Some(7),
            output_format: "webp".into(),
        }
    }

    const KEEP: Duration = Duration::from_secs(300);

    fn generate(servers: &WarmImageServers, model: &str) -> Result<Vec<u8>> {
        servers.generate(
            Path::new("/bin/sd-server"),
            key(model),
            &["--steps".into(), "4".into()],
            KEEP,
            &request(),
        )
    }

    #[test]
    fn starts_once_and_reuses_the_server_for_the_same_key() {
        let (servers, script) = servers();
        assert_eq!(generate(&servers, "flux").unwrap(), vec![1, 2, 3]);
        assert_eq!(generate(&servers, "flux").unwrap(), vec![1, 2, 3]);
        assert_eq!(script.spawned.load(Ordering::SeqCst), 1);
        assert_eq!(script.generated.load(Ordering::SeqCst), 2);
        assert!(servers.holds("flux"));
        assert!(!servers.holds("z-image"));
    }

    #[test]
    fn passes_loopback_and_the_start_arguments() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        let args = script.spawn_args.lock()[0].clone();
        assert_eq!(
            args,
            vec![
                OsString::from("--listen-ip"),
                "127.0.0.1".into(),
                "--listen-port".into(),
                "40000".into(),
                "--steps".into(),
                "4".into(),
            ]
        );
    }

    #[test]
    fn replaces_the_server_for_another_key() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        generate(&servers, "z-image").unwrap();
        assert_eq!(script.spawned.load(Ordering::SeqCst), 2);
        assert_eq!(script.killed.load(Ordering::SeqCst), 1);
        assert!(servers.holds("z-image"));
    }

    #[test]
    fn expires_after_keep_warm_and_not_before() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        let now = Instant::now();
        assert!(!servers.expire_idle(now + Duration::from_secs(10)));
        assert!(servers.expire_idle(now + KEEP + Duration::from_secs(1)));
        assert_eq!(script.killed.load(Ordering::SeqCst), 1);
        assert!(!servers.holds("flux"));
        assert!(!servers.expire_idle(now + KEEP * 2));
    }

    #[test]
    fn evicts_on_request_and_says_whether_one_ran() {
        let (servers, script) = servers();
        assert!(!servers.evict("paused"));
        generate(&servers, "flux").unwrap();
        assert!(servers.evict("paused"));
        assert_eq!(script.killed.load(Ordering::SeqCst), 1);
        assert!(!servers.holds("flux"));
    }

    #[test]
    fn a_failed_picture_stops_the_server_and_errs() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        script.fail_generate.store(true, Ordering::SeqCst);
        let error = generate(&servers, "flux").unwrap_err();
        assert!(
            format!("{error:#}").contains("connection reset"),
            "{error:#}"
        );
        assert_eq!(script.killed.load(Ordering::SeqCst), 1);
        assert!(!servers.holds("flux"));
    }

    #[test]
    fn a_server_that_exits_while_starting_errs_with_its_stderr_after_one_retry() {
        let (servers, script) = servers();
        script.exits_while_starting.store(true, Ordering::SeqCst);
        let error = generate(&servers, "flux").unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("exited while starting"), "{text}");
        assert!(text.contains("model file not found"), "{text}");
        assert_eq!(script.spawned.load(Ordering::SeqCst), 2);
        assert!(!servers.holds("flux"));
    }

    #[test]
    fn a_server_never_ready_times_out_and_is_killed() {
        let (servers, script) = servers();
        script.never_ready.store(true, Ordering::SeqCst);
        let error = generate(&servers, "flux").unwrap_err();
        assert!(
            format!("{error:#}").contains("not ready within"),
            "{error:#}"
        );
        assert_eq!(script.killed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_server_that_ended_on_its_own_is_started_again() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        script.exits_while_starting.store(false, Ordering::SeqCst);
        if let Some(running) = servers.running.lock().as_mut() {
            running.process = Box::new(FakeProcess {
                script: Arc::clone(&script),
                exits: true,
            });
        }
        generate(&servers, "flux").unwrap();
        assert_eq!(script.spawned.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropping_the_servers_stops_the_running_one() {
        let (servers, script) = servers();
        generate(&servers, "flux").unwrap();
        drop(servers);
        assert_eq!(script.killed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_body_carries_every_per_job_value() {
        let mut request = request();
        request.negative_prompt = Some("blurry".into());
        let body = request.body();
        assert_eq!(body["n"], 1);
        assert_eq!(body["size"], "512x512");
        assert_eq!(body["output_format"], "webp");
        let prompt = body["prompt"].as_str().unwrap();
        let (text, rest) = prompt.split_once(" <sd_cpp_extra_args>").unwrap();
        assert_eq!(text, "a lighthouse");
        let extra: Value =
            serde_json::from_str(rest.strip_suffix("</sd_cpp_extra_args>").unwrap()).unwrap();
        assert_eq!(extra["seed"], 7);
        assert_eq!(extra["width"], 512);
        assert_eq!(extra["negative_prompt"], "blurry");
        assert_eq!(extra["sample_params"]["sample_steps"], 4);
        assert_eq!(extra["sample_params"]["sample_method"], "euler");
        assert_eq!(extra["sample_params"]["guidance"]["txt_cfg"], 1.0);
    }

    #[test]
    fn the_body_asks_a_random_seed_and_no_negative_prompt_when_unset() {
        let mut request = request();
        request.seed = None;
        request.negative_prompt = Some(String::new());
        request.sampling_method = None;
        let prompt = request.body()["prompt"].as_str().unwrap().to_string();
        assert!(prompt.contains("\"seed\":-1"), "{prompt}");
        assert!(!prompt.contains("negative_prompt"), "{prompt}");
        assert!(!prompt.contains("sample_method"), "{prompt}");
    }

    #[test]
    fn decodes_the_picture_and_refuses_a_missing_or_broken_one() {
        let ok = json!({ "data": [{ "b64_json": "AQID" }] });
        assert_eq!(picture_of(&ok).unwrap(), vec![1, 2, 3]);
        assert!(picture_of(&json!({ "data": [] })).is_err());
        assert!(picture_of(&json!({ "data": [{ "b64_json": "%%%" }] })).is_err());
        assert!(picture_of(&json!({ "data": [{ "b64_json": "" }] })).is_err());
    }

    #[test]
    fn finds_sd_server_beside_sd_cli_only_when_it_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let sd_cli = dir.path().join("sd-cli");
        std::fs::write(&sd_cli, b"").unwrap();
        assert_eq!(sibling_sd_server(&sd_cli), None);
        let name = if cfg!(windows) {
            "sd-server.exe"
        } else {
            "sd-server"
        };
        std::fs::write(dir.path().join(name), b"").unwrap();
        assert_eq!(sibling_sd_server(&sd_cli), Some(dir.path().join(name)));
    }

    #[test]
    fn nothing_started_means_nothing_to_evict() {
        // The process-wide instance is only made by `warm_servers()`, which no unit test calls.
        assert!(!evict_started("paused"));
        assert!(!StartedWarm.holds("flux"));
        assert!(!StartedWarm.evict("paused"));
    }

    #[test]
    fn the_servers_answer_admission_through_warm_hold() {
        let (servers, _script) = servers();
        generate(&servers, "flux").unwrap();
        let hold: &dyn WarmHold = &servers;
        assert!(hold.holds("flux"));
        assert!(hold.evict("load"));
        assert!(!hold.holds("flux"));
    }

    mod http {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn port_of(server: &MockServer) -> u16 {
            server.address().port()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn ready_is_true_only_on_200() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
                .mount(&server)
                .await;
            let port = port_of(&server);
            let ready = tokio::task::spawn_blocking(move || HttpClient::new().ready(port))
                .await
                .unwrap();
            assert!(ready);

            let down = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&down)
                .await;
            let port = port_of(&down);
            let ready = tokio::task::spawn_blocking(move || HttpClient::new().ready(port))
                .await
                .unwrap();
            assert!(!ready);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn generate_posts_the_body_and_decodes_the_picture() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/images/generations"))
                .and(wiremock::matchers::body_partial_json(
                    json!({ "n": 1, "size": "512x512" }),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({ "data": [{ "b64_json": "AQID" }] })),
                )
                .mount(&server)
                .await;
            let port = port_of(&server);
            let bytes = tokio::task::spawn_blocking(move || {
                HttpClient::new().generate(port, &request().body(), Duration::from_secs(5))
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(bytes, vec![1, 2, 3]);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn generate_names_a_failing_status() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500).set_body_string("out of memory"))
                .mount(&server)
                .await;
            let port = port_of(&server);
            let error = tokio::task::spawn_blocking(move || {
                HttpClient::new().generate(port, &request().body(), Duration::from_secs(5))
            })
            .await
            .unwrap()
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("500") && text.contains("out of memory"),
                "{text}"
            );
        }
    }
}
