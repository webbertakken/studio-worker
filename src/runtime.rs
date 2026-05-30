//! Long-running auto-update task + one-shot CLI helpers.
//!
//! After the WS migration the runtime owns just two background
//! tasks: the WebSocket session (`ws::session::spawn_ws_session`,
//! which subsumes heartbeats, claim/accept/complete, fail, and log
//! shipping) and the auto-updater (`spawn_auto_updater`).  Per-tick
//! helpers from the old polling loops are gone.
use crate::{
    config::{self, Config, SharedConfig},
    engine::Engine,
    sys,
    types::*,
    update, AGENT_VERSION,
};
use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{info, warn};

/// Tracing target for runtime-level events (startup, state mutations).
/// Stable so operators can filter with `RUST_LOG=studio_worker::runtime=debug`.
const TRACE_TARGET: &str = "studio_worker::runtime";

/// Maximum number of finished jobs kept in `WorkerObservers::recent_jobs`.
/// Older entries fall off the back of the ring.
pub const RECENT_JOBS_CAP: usize = 50;

/// Maximum number of log entries kept in `WorkerObservers::recent_logs`
/// for the UI's Logs tab.  The shipping queue (`logs: Arc<Mutex<Vec<…>>>`)
/// is drained on every WS tick — the display ring is what the UI reads.
pub const RECENT_LOGS_CAP: usize = 1000;

/// Prompt previews stored in `CurrentJob` / `RecentJob` are clipped to
/// this many chars so the in-memory state stays bounded even when LLM
/// prompts are huge.
pub const PROMPT_PREVIEW_CHARS: usize = 200;

/// Job in flight right now.  Populated by the WS session before
/// dispatch, cleared once the job finishes (success or failure).
#[derive(Debug, Clone)]
pub struct CurrentJob {
    pub job_id: String,
    pub kind: TaskKind,
    pub model: String,
    pub prompt: String,
    pub started_at: DateTime<Utc>,
}

/// Outcome a finished job ended with.  Failures carry the human
/// reason (already surfaced to logs + Sentry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Completed,
    Failed { reason: String },
}

/// One finished job, retained in the recent-jobs ring for the UI.
#[derive(Debug, Clone)]
pub struct RecentJob {
    pub job_id: String,
    pub kind: TaskKind,
    pub model: String,
    pub prompt: String,
    pub outcome: JobOutcome,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// Result of the most recent heartbeat the WS session sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    Ok,
    Err { reason: String },
}

#[derive(Debug, Clone)]
pub struct HeartbeatStatus {
    pub last_attempt_at: DateTime<Utc>,
    pub outcome: HeartbeatOutcome,
}

/// Bundle of in-process observation slots the WS session writes to and
/// the optional native UI reads from.  `Default` gives empty slots so
/// existing (headless) call sites stay one-liners.  Cheap to clone —
/// every field is an `Arc`.
#[derive(Clone, Default)]
pub struct WorkerObservers {
    pub current_job: Arc<Mutex<Option<CurrentJob>>>,
    pub recent_jobs: Arc<Mutex<VecDeque<RecentJob>>>,
    pub last_heartbeat: Arc<Mutex<Option<HeartbeatStatus>>>,
    /// Bounded ring of every log entry the worker has emitted, kept
    /// for the UI's Logs tab.  Separate from the WS ship queue
    /// (which is drained every second) so the display doesn't blank
    /// out between ticks.
    pub recent_logs: Arc<Mutex<VecDeque<LogEntry>>>,
}

pub fn truncate_prompt(s: &str) -> String {
    if s.chars().count() <= PROMPT_PREVIEW_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(PROMPT_PREVIEW_CHARS).collect();
    out.push('…');
    out
}

pub fn record_recent_job(observers: &WorkerObservers, entry: RecentJob) {
    let mut ring = observers.recent_jobs.lock();
    ring.push_front(entry);
    while ring.len() > RECENT_JOBS_CAP {
        ring.pop_back();
    }
}

/// Test-only helper to populate the recent-jobs ring without driving a
/// full claim cycle.  Lives in the library surface so integration
/// tests can pin the ring-capacity contract cheaply.
#[doc(hidden)]
pub fn push_recent_job_for_tests(observers: &WorkerObservers, job_id: &str) {
    let now = Utc::now();
    record_recent_job(
        observers,
        RecentJob {
            job_id: job_id.to_string(),
            kind: TaskKind::Image,
            model: "synthetic".into(),
            prompt: String::new(),
            outcome: JobOutcome::Completed,
            started_at: now,
            finished_at: now,
        },
    );
}

pub const AUTO_UPDATE_TICK: Duration = Duration::from_secs(60);
/// Cadence at which the auto-updater's idle wait re-checks the `stop`
/// flag.  Mirrors the WS session's shutdown tick so a SIGTERM / SIGINT
/// landing during the (up to `AUTO_UPDATE_TICK`-long) idle window wakes
/// the loop within ~250 ms instead of leaving `run_loops`' join blocked
/// for a whole tick.
pub const AUTO_UPDATE_SHUTDOWN_TICK: Duration = Duration::from_millis(250);
/// Default WS heartbeat interval, re-exported here so the native UI
/// (and any other downstream readers) get a stable constant without
/// reaching into `ws::session`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Schedule for the auto-updater loop.  The WS session has its own
/// `SessionSchedule` (see `ws::session`).
#[derive(Debug, Clone, Copy)]
pub struct LoopSchedule {
    pub auto_update_tick: Duration,
    /// How often the idle wait between update checks re-polls the
    /// `stop` flag, so a shutdown request isn't deferred for a whole
    /// `auto_update_tick`.
    pub shutdown_tick: Duration,
}

impl Default for LoopSchedule {
    fn default() -> Self {
        Self {
            auto_update_tick: AUTO_UPDATE_TICK,
            shutdown_tick: AUTO_UPDATE_SHUTDOWN_TICK,
        }
    }
}

impl LoopSchedule {
    /// Schedule with 1 ms intervals — used by tests to exercise the
    /// loop wrappers without blocking.
    pub fn fast_for_tests() -> Self {
        Self {
            auto_update_tick: Duration::from_millis(1),
            shutdown_tick: Duration::from_millis(1),
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot helpers used by the CLI subcommands
// ---------------------------------------------------------------------------

/// Bundle of flags from `studio-worker register`.
#[derive(Debug, Clone, Default)]
pub struct RegisterArgs {
    pub api_base_url: Option<String>,
    pub reset: bool,
}

/// Persist registration metadata for the next launch.  No HTTP — the
/// auto-register orchestration inside `run` / `ui` is the only thing
/// that talks to the studio.
pub async fn register(config_path: Option<&str>, args: RegisterArgs) -> Result<()> {
    let (mut cfg, path) = config::load(config_path)?;

    if args.reset {
        cfg.worker_id = None;
        cfg.auth_token = None;
        cfg.registration_request_id = None;
        cfg.registration_secret = None;
        cfg.install_id = None;
    }
    if let Some(url) = args.api_base_url {
        cfg.api_base_url = url;
    }

    config::save(&cfg, &path)?;
    if args.reset {
        info!(
            config_path = %path.display(),
            "local registration state cleared; next launch will auto-register"
        );
        println!(
            "local registration state cleared; run `studio-worker run` or \
             `studio-worker ui` to auto-register"
        );
    } else {
        info!(
            config_path = %path.display(),
            "register flags persisted; next launch will auto-register"
        );
        println!(
            "saved; run `studio-worker run` or `studio-worker ui` to auto-register against {}",
            cfg.api_base_url
        );
    }
    Ok(())
}

pub async fn status(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    println!("{}", format_status(&cfg, &path));
    Ok(())
}

pub fn format_status(cfg: &Config, path: &std::path::Path) -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "config path:        {}", path.display());
    let _ = writeln!(out, "api_base_url:       {}", cfg.api_base_url);
    let registration_line = if cfg.worker_id.is_some() && cfg.auth_token.is_some() {
        format!("approved as {}", cfg.worker_id.as_deref().unwrap_or(""))
    } else if let Some(rid) = cfg.registration_request_id.as_deref() {
        format!("pending operator approval (request {rid})")
    } else {
        "not registered (will auto-register on next launch)".into()
    };
    let _ = writeln!(out, "registration:       {registration_line}");
    let _ = writeln!(out, "vram_threshold_gb:  {}", cfg.vram_threshold_gb);
    let _ = writeln!(out, "auto_start:         {}", cfg.auto_start);
    let _ = writeln!(out, "models_root:        {}", cfg.models_root.display());
    let _ = writeln!(out, "auto_update:        {}", cfg.auto_update_enabled);
    let _ = writeln!(
        out,
        "update_interval:    {}s",
        cfg.auto_update_interval_secs
    );
    out
}

pub fn set_threshold(config_path: Option<&str>, gb: f32) -> Result<()> {
    if gb < 0.0 {
        return Err(anyhow!("threshold must be >= 0"));
    }
    let (mut cfg, path) = config::load(config_path)?;
    cfg.vram_threshold_gb = gb;
    config::save(&cfg, &path)?;
    info!(
        target: TRACE_TARGET,
        op = "set_threshold",
        vram_threshold_gb = gb,
        config_path = path.display().to_string(),
        "VRAM threshold persisted"
    );
    println!("vram_threshold_gb = {gb}");
    Ok(())
}

/// Emit a one-shot startup banner so operators can confirm which
/// config the worker actually loaded.  Without this the only thing in
/// `journalctl -u studio-worker` on a healthy boot is whatever the
/// loops happen to log on their first tick.
pub fn log_startup_banner(cfg: &Config, path: &std::path::Path) {
    info!(
        target: TRACE_TARGET,
        op = "startup",
        version = AGENT_VERSION,
        config_path = path.display().to_string(),
        api_base_url = cfg.api_base_url.as_str(),
        vram_threshold_gb = cfg.vram_threshold_gb,
        auto_start = cfg.auto_start,
        auto_update_enabled = cfg.auto_update_enabled,
        auto_update_interval_secs = cfg.auto_update_interval_secs,
        models_root = cfg.models_root.display().to_string(),
        worker_id = cfg.worker_id.as_deref().unwrap_or("(unregistered)"),
        "studio-worker booting"
    );
}

pub fn show_config(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    println!("# {}", path.display());
    print!("{}", toml::to_string_pretty(&cfg)?);
    Ok(())
}

pub async fn check_update(config_path: Option<&str>) -> Result<()> {
    let (cfg, _) = config::load(config_path)?;
    let current = semver::Version::parse(AGENT_VERSION)
        .map_err(|e| anyhow!("invalid current version {AGENT_VERSION}: {e}"))?;
    let outcome = tokio::task::spawn_blocking(move || {
        update::check(&cfg.auto_update_feed, &current, cfg.auto_update_prerelease)
    })
    .await??;
    println!("{}", format_check_outcome(&outcome));
    Ok(())
}

pub fn format_check_outcome(outcome: &update::CheckOutcome) -> String {
    match outcome {
        update::CheckOutcome::UpToDate { current } => format!("up to date: {current}"),
        update::CheckOutcome::NewerAvailable { current, latest } => {
            format!("update available: {current} -> {latest}")
        }
    }
}

// ---------------------------------------------------------------------------
// Long-running run loop
// ---------------------------------------------------------------------------

pub async fn run(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    log_startup_banner(&cfg, &path);

    let cfg = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    // Operator pause toggle.  Runtime-only — never persisted, so the
    // worker comes up unpaused after every restart.
    let paused = Arc::new(AtomicBool::new(false));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let observers = WorkerObservers::default();
    let registration = crate::auto_register::shared_initial();

    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let signal = wait_for_shutdown_signal().await;
        request_shutdown(&stop_clone, signal);
    });

    // Block on auto-register until the operator approves (or rejects).
    // Polls every 30s; aborts on Ctrl-C.
    ensure_registered(&cfg, &path, &registration, &stop).await?;

    run_loops(
        cfg,
        stop,
        logs,
        busy,
        paused,
        observers,
        LoopSchedule::default(),
    )
    .await
}

/// Flip the `stop` flag and emit a shutdown breadcrumb so an operator
/// tailing the journal sees a clean stop, mirroring
/// [`log_startup_banner`].  Pulled out of the signal task so the
/// shutdown decision is unit-testable without delivering a real OS
/// signal.  `signal` names whatever woke us (e.g. `"SIGTERM"`).
pub fn request_shutdown(stop: &AtomicBool, signal: &str) {
    let already_stopping = stop.swap(true, Ordering::SeqCst);
    info!(
        target: TRACE_TARGET,
        op = "shutdown",
        signal,
        already_stopping,
        "shutdown signal received; stopping worker gracefully"
    );
}

/// Block until the OS asks the worker to stop, returning the name of
/// the signal that fired.
///
/// On Unix we wait on **both** SIGINT (interactive Ctrl-C) and SIGTERM.
/// SIGTERM is the signal `systemctl stop` / `launchctl unload` / host
/// shutdown deliver by default, and the worker ships as a `Type=simple`
/// systemd unit (see `service::render_service`).  Listening for Ctrl-C
/// alone meant the service manager's stop never reached the graceful
/// path: the WS session was killed mid-`close`, the studio saw an
/// abrupt disconnect, and the final log batch never flushed.  If the
/// SIGTERM handler can't be installed we degrade to Ctrl-C only rather
/// than abort the shutdown task.
///
/// On non-Unix we wait on Ctrl-C, which tokio maps to the console
/// Ctrl-C / close events.
#[cfg_attr(coverage_nightly, coverage(off))]
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    target: TRACE_TARGET,
                    op = "shutdown",
                    error = %e,
                    "could not install SIGTERM handler; falling back to Ctrl-C only"
                );
                let _ = tokio::signal::ctrl_c().await;
                return "SIGINT";
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

/// Loop auto_register::tick on a 30s cadence until `worker_id` +
/// `auth_token` are populated (Approved) or the operator rejects.
pub async fn ensure_registered(
    cfg: &SharedConfig,
    path: &std::path::Path,
    registration: &crate::auto_register::SharedRegistration,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    use std::time::Duration;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(anyhow!("shutdown before registration completed"));
        }
        {
            let snap = cfg.lock();
            if snap.worker_id.is_some() && snap.auth_token.is_some() {
                return Ok(());
            }
        }
        let state = crate::auto_register::tick(cfg, path, registration).await;
        match state {
            crate::auto_register::RegistrationState::Approved => return Ok(()),
            crate::auto_register::RegistrationState::Rejected { reason } => {
                return Err(anyhow!(
                    "registration rejected by the studio operator: {reason}.  \
                     Run `studio-worker register --reset` to clear local state \
                     and submit a fresh request."
                ));
            }
            _ => {}
        }
        // Sleep with a fast-cancel on stop.
        for _ in 0..30 {
            if stop.load(Ordering::SeqCst) {
                return Err(anyhow!("shutdown during registration wait"));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Spawn the WS session + auto-updater, wait for them.  Pulled out of
/// `run` so tests can drive with a different schedule.
///
/// `paused` is the runtime-only Pause / Resume toggle the UI flips.
/// When set, the WS session advertises `auto_enabled = false` in
/// heartbeats and refuses new job offers without restarting the
/// session.
pub async fn run_loops(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    observers: WorkerObservers,
    schedule: LoopSchedule,
) -> Result<()> {
    let session_schedule = crate::ws::session::SessionSchedule::default();
    let session = crate::ws::session::spawn_ws_session(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
        paused.clone(),
        observers.clone(),
        session_schedule,
    );
    let auto_updater = spawn_auto_updater(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
        schedule,
    );
    let (session_result, _) = tokio::join!(session, auto_updater);
    session_result
}

// ---------------------------------------------------------------------------
// Per-tick helpers — pure async fns, easy to drive from unit tests.
// ---------------------------------------------------------------------------

// (The old per-tick HTTP helpers — heartbeat_tick, claim_tick, log_shipper_tick,
//  run_job, ClaimOutcome — lived here.  They are gone with the WS migration.
//  See `ws::session::spawn_ws_session` for the replacement that runs the
//  whole session in one connected loop.)

/// What the auto-updater decided this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateDecision {
    /// Auto-update is turned off — do nothing.
    Disabled,
    /// Worker is currently running a job — skip.
    SkippedBusy,
    /// Local version is already the latest.
    UpToDate,
    /// Check failed (network etc.) — leave a log entry, try again later.
    CheckError(String),
    /// A newer version was applied successfully.  Caller should restart.
    Updated,
    /// A newer version was found but the install failed.
    UpdateError(String),
}

pub async fn auto_update_tick(
    cfg: &Config,
    busy: bool,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
) -> AutoUpdateDecision {
    if !cfg.auto_update_enabled {
        return AutoUpdateDecision::Disabled;
    }
    if busy {
        push_log(
            logs,
            "info",
            "auto-update",
            "skipping check: worker is busy on a job",
            None,
        );
        return AutoUpdateDecision::SkippedBusy;
    }
    let feed = cfg.auto_update_feed.clone();
    let prerelease = cfg.auto_update_prerelease;
    let logs_for_task = logs.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<AutoUpdateDecision> {
        let current = semver::Version::parse(AGENT_VERSION)
            .map_err(|e| anyhow!("invalid AGENT_VERSION {AGENT_VERSION}: {e}"))?;
        match update::check(&feed, &current, prerelease) {
            Ok(update::CheckOutcome::UpToDate { current }) => {
                push_log(
                    &logs_for_task,
                    "info",
                    "auto-update",
                    &format!("up to date at {current}"),
                    None,
                );
                Ok(AutoUpdateDecision::UpToDate)
            }
            Ok(update::CheckOutcome::NewerAvailable { current, latest }) => {
                push_log(
                    &logs_for_task,
                    "info",
                    "auto-update",
                    &format!("update available {current} -> {latest}; applying"),
                    None,
                );
                match update::apply(&feed, &latest) {
                    Ok(()) => {
                        push_log(
                            &logs_for_task,
                            "info",
                            "auto-update",
                            "binary replaced; restart pending",
                            None,
                        );
                        Ok(AutoUpdateDecision::Updated)
                    }
                    Err(e) => {
                        push_log(
                            &logs_for_task,
                            "error",
                            "auto-update",
                            &format!("update failed: {e}"),
                            None,
                        );
                        Ok(AutoUpdateDecision::UpdateError(e.to_string()))
                    }
                }
            }
            Err(e) => {
                push_log(
                    &logs_for_task,
                    "warn",
                    "auto-update",
                    &format!("check failed: {e}"),
                    None,
                );
                Ok(AutoUpdateDecision::CheckError(e.to_string()))
            }
        }
    })
    .await;
    match outcome {
        Ok(Ok(decision)) => decision,
        Ok(Err(e)) => AutoUpdateDecision::CheckError(e.to_string()),
        Err(e) => AutoUpdateDecision::CheckError(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Long-running task wrappers — they exist solely to call the ticks in a
// loop on a schedule.  All real logic lives in the ticks.
// ---------------------------------------------------------------------------

// (`spawn_heartbeat`, `spawn_claim_loop`, `spawn_log_shipper`, and
//  `next_delay_for` lived here.  Their behaviour is now carried by the
//  WS-driven tasks in `ws::session`.)

/// Sleep up to `total`, re-checking `stop` every `tick` and returning
/// the instant a shutdown is requested.  Keeps long idle waits (the
/// auto-update tick here, reconnect backoff in the WS session)
/// responsive to SIGTERM / SIGINT without busy-looping.  Shared by the
/// runtime auto-updater and `ws::session`.
pub(crate) async fn wait_with_stop(total: Duration, stop: &Arc<AtomicBool>, tick: Duration) {
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let next = tick.min(total - elapsed);
        tokio::time::sleep(next).await;
        elapsed += next;
    }
}

pub fn spawn_auto_updater(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    schedule: LoopSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut elapsed = Duration::from_secs(0);
        while !stop.load(Ordering::SeqCst) {
            // Stop-aware idle wait: a shutdown signal during this window
            // wakes the loop within `schedule.shutdown_tick` instead of
            // leaving `run_loops`' join() blocked for a full
            // `auto_update_tick`.
            wait_with_stop(schedule.auto_update_tick, &stop, schedule.shutdown_tick).await;
            if stop.load(Ordering::SeqCst) {
                break;
            }
            elapsed += schedule.auto_update_tick;
            let snapshot = cfg.lock().clone();
            if elapsed < Duration::from_secs(snapshot.auto_update_interval_secs) {
                continue;
            }
            elapsed = Duration::from_secs(0);
            let busy_now = busy.load(Ordering::SeqCst);
            let decision = auto_update_tick(&snapshot, busy_now, &logs).await;
            if matches!(decision, AutoUpdateDecision::Updated) {
                stop.store(true, Ordering::SeqCst);
                update::restart_self();
            }
        }
    })
}

// (`run_job` lived here.  See `ws::session::run_offered_job` for the
//  WS-driven replacement.)

pub fn prompt_for(task: &Task) -> String {
    match task {
        Task::Image(p) => p.prompt.clone(),
        Task::Llm(p) => p
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default(),
        Task::AudioStt(p) => p.input_url.clone(),
        Task::AudioTts(p) => p.text.clone(),
        Task::Video(p) => p.prompt.clone(),
    }
}

pub fn is_unsupported_kind(e: &anyhow::Error) -> bool {
    e.to_string().contains("cannot serve")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn build_capabilities(cfg: &Config, engine: &dyn Engine) -> WorkerCapabilities {
    build_capabilities_with(cfg, engine, true)
}

/// Same as [`build_capabilities`] but lets the caller drive
/// `auto_enabled` from a runtime pause flag (the UI's Pause/Resume
/// button).  The persisted [`Config`] no longer carries that bit —
/// it's an in-process toggle.
pub fn build_capabilities_with(
    cfg: &Config,
    engine: &dyn Engine,
    auto_enabled: bool,
) -> WorkerCapabilities {
    let vram = sys::detect_vram_gb().unwrap_or(0.0);
    let caps = engine.capabilities();
    let supported_models_per_kind = caps.supported_models_per_kind.clone();
    let task_kinds = caps.kinds();
    // Legacy `supported_models` is a flat list across all kinds so the
    // studio API's claim filter (which only knows about this field) can
    // match jobs of any modality this worker can serve.
    let supported_models = {
        let mut all = caps.flat_models();
        all.sort();
        all.dedup();
        all
    };

    WorkerCapabilities {
        machine_name: sys::machine_name(),
        username: sys::username(),
        agent_version: AGENT_VERSION.to_string(),
        engine: engine.name().to_string(),
        vram_total_gb: vram,
        vram_threshold_gb: cfg.vram_threshold_gb,
        auto_enabled,
        auto_start: cfg.auto_start,
        supported_models,
        task_kinds,
        supported_models_per_kind,
    }
}

/// One-line, operator-facing summary of what this worker advertises to
/// the studio on the WS handshake.  Logged once per session attempt so
/// the worker's own logs (and the studio's shipped-log view) record
/// exactly which task kinds, models, and VRAM budget were offered — the
/// missing complement to [`log_startup_banner`], which only covers the
/// loaded config.  Without it, an operator chasing "why won't my worker
/// claim image jobs" has no record of what the worker told the studio
/// it could do.  Pure so the formatting is unit-tested without a live
/// session.
pub fn summarize_capabilities(caps: &WorkerCapabilities) -> String {
    let kinds = caps
        .task_kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "advertising engine={}, vram={:.1}/{:.1}GB threshold, auto_enabled={}, \
         kinds=[{}], {} model(s)=[{}]",
        caps.engine,
        caps.vram_total_gb,
        caps.vram_threshold_gb,
        caps.auto_enabled,
        kinds,
        caps.supported_models.len(),
        caps.supported_models.join(", "),
    )
}

pub fn push_log(
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    level: &str,
    category: &str,
    message: &str,
    job_id: Option<String>,
) {
    push_log_with_observers(logs, None, level, category, message, job_id);
}

/// Same as [`push_log`] but also appends to
/// [`WorkerObservers::recent_logs`] so the UI's Logs tab keeps a
/// rolling display window.  The WS session uses this variant so
/// operators don't see the Logs tab blank out every second when the
/// shipping queue gets drained.
pub fn push_log_with_observers(
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    observers: Option<&WorkerObservers>,
    level: &str,
    category: &str,
    message: &str,
    job_id: Option<String>,
) {
    let entry = LogEntry {
        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        level: level.to_string(),
        category: category.to_string(),
        message: message.to_string(),
        job_id,
    };
    if level == "error" {
        tracing::error!(target: "studio_worker", "[{category}] {message}");
    } else if level == "warn" {
        tracing::warn!(target: "studio_worker", "[{category}] {message}");
    } else {
        info!(target: "studio_worker", "[{category}] {message}");
    }
    logs.lock().push(entry.clone());
    if let Some(o) = observers {
        let mut ring = o.recent_logs.lock();
        ring.push_back(entry);
        while ring.len() > RECENT_LOGS_CAP {
            ring.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::SyntheticEngine;

    #[test]
    fn capabilities_advertises_all_synthetic_kinds() {
        let cfg = Config::default();
        let engine = SyntheticEngine::new();
        let cap = build_capabilities(&cfg, &engine);
        assert_eq!(cap.engine, "synthetic");
        assert_eq!(cap.task_kinds.len(), TaskKind::ALL.len());
        assert!(cap.auto_enabled, "default capability snapshot is unpaused");
        for kind in TaskKind::ALL {
            assert!(cap.supported_models_per_kind.contains_key(&kind));
        }
    }

    #[test]
    fn capabilities_with_paused_flag_drives_auto_enabled() {
        let cfg = Config::default();
        let engine = SyntheticEngine::new();
        let paused_caps = build_capabilities_with(&cfg, &engine, false);
        assert!(!paused_caps.auto_enabled);
    }

    #[test]
    fn summarize_capabilities_lists_engine_kinds_models_vram_and_pause_state() {
        let cfg = Config {
            vram_threshold_gb: 6.0,
            ..Config::default()
        };
        let engine = SyntheticEngine::new();
        let caps = build_capabilities_with(&cfg, &engine, true);
        let summary = summarize_capabilities(&caps);
        // Engine name + every advertised kind is present.
        assert!(summary.contains("engine=synthetic"), "got: {summary}");
        for kind in &caps.task_kinds {
            assert!(
                summary.contains(kind.as_str()),
                "missing kind {} in: {summary}",
                kind.as_str()
            );
        }
        // Model count + an actual advertised model id are present.
        assert!(
            summary.contains(&format!("{} model(s)", caps.supported_models.len())),
            "missing model count in: {summary}"
        );
        assert!(
            summary.contains("synthetic"),
            "missing model id in: {summary}"
        );
        // VRAM budget (total/threshold) + unpaused state are visible.
        assert!(
            summary.contains("6.0"),
            "missing vram threshold in: {summary}"
        );
        assert!(summary.contains("auto_enabled=true"), "got: {summary}");
    }

    #[test]
    fn summarize_capabilities_reflects_paused_state() {
        let cfg = Config::default();
        let engine = SyntheticEngine::new();
        let caps = build_capabilities_with(&cfg, &engine, false);
        assert!(
            summarize_capabilities(&caps).contains("auto_enabled=false"),
            "paused worker must advertise auto_enabled=false"
        );
    }

    #[test]
    fn prompt_for_extracts_per_kind() {
        let image = Task::Image(ImageParams {
            prompt: "a stone golem".into(),
            ..Default::default()
        });
        assert_eq!(prompt_for(&image), "a stone golem");

        let llm = Task::Llm(LlmParams {
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "be helpful".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                },
            ],
            max_tokens: 32,
            temperature: 0.5,
            ..Default::default()
        });
        assert_eq!(prompt_for(&llm), "hi");

        let llm_empty = Task::Llm(LlmParams {
            messages: vec![],
            ..Default::default()
        });
        assert_eq!(prompt_for(&llm_empty), "");

        let stt = Task::AudioStt(AudioSttParams {
            input_url: "https://example.com/clip.wav".into(),
            ..Default::default()
        });
        assert_eq!(prompt_for(&stt), "https://example.com/clip.wav");

        let tts = Task::AudioTts(AudioTtsParams {
            text: "hi there".into(),
            voice: "v".into(),
            ext: "wav".into(),
            ..Default::default()
        });
        assert_eq!(prompt_for(&tts), "hi there");

        let video = Task::Video(VideoParams {
            prompt: "a tiny dragon".into(),
            seconds: 1.0,
            width: 256,
            height: 256,
            ext: "mp4".into(),
            ..Default::default()
        });
        assert_eq!(prompt_for(&video), "a tiny dragon");
    }

    #[test]
    fn is_unsupported_kind_matches_engine_message() {
        let err = anyhow!("multi engine cannot serve llm tasks");
        assert!(is_unsupported_kind(&err));
        let other = anyhow!("network timeout");
        assert!(!is_unsupported_kind(&other));
    }

    #[test]
    fn format_status_includes_every_field() {
        let cfg = Config::default();
        let out = format_status(&cfg, std::path::Path::new("/tmp/x.toml"));
        assert!(out.contains("config path:"));
        assert!(out.contains("api_base_url:"));
        assert!(out.contains("registration:"));
        assert!(out.contains("not registered"));
        assert!(out.contains("models_root:"));
        assert!(out.contains("auto_update:"));
        assert!(out.contains("update_interval:"));
    }

    #[test]
    fn format_status_shows_worker_id_when_registered() {
        let cfg = Config {
            worker_id: Some("w-abc".into()),
            auth_token: Some("tok".into()),
            ..Config::default()
        };
        let out = format_status(&cfg, std::path::Path::new("/tmp/x.toml"));
        assert!(out.contains("w-abc"));
        assert!(out.contains("approved"));
    }

    #[test]
    fn format_status_shows_pending_request_id() {
        let cfg = Config {
            registration_request_id: Some("rr-7".into()),
            ..Config::default()
        };
        let out = format_status(&cfg, std::path::Path::new("/tmp/x.toml"));
        assert!(out.contains("pending operator approval"));
        assert!(out.contains("rr-7"));
    }

    #[test]
    fn format_check_outcome_handles_both_branches() {
        let up = update::CheckOutcome::UpToDate {
            current: semver::Version::new(1, 2, 3),
        };
        assert!(format_check_outcome(&up).contains("up to date"));
        let newer = update::CheckOutcome::NewerAvailable {
            current: semver::Version::new(1, 2, 3),
            latest: semver::Version::new(1, 3, 0),
        };
        let s = format_check_outcome(&newer);
        assert!(s.contains("1.2.3 -> 1.3.0"));
    }

    #[test]
    fn push_log_appends_an_entry() {
        let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
        push_log(&logs, "info", "test", "hi", None);
        push_log(&logs, "warn", "test", "wat", Some("j-1".into()));
        push_log(&logs, "error", "test", "boom", None);
        let v = logs.lock();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].level, "info");
        assert_eq!(v[1].level, "warn");
        assert_eq!(v[1].job_id.as_deref(), Some("j-1"));
        assert_eq!(v[2].level, "error");
    }

    // --- async tick tests ---

    #[test]
    fn request_shutdown_sets_the_stop_flag() {
        let stop = AtomicBool::new(false);
        request_shutdown(&stop, "SIGTERM");
        assert!(stop.load(Ordering::SeqCst));
    }

    #[test]
    fn request_shutdown_reconfirms_when_already_stopping() {
        // A second signal (or a race with another shutdown path) must
        // not panic or clear the flag — it just re-confirms the stop.
        let stop = AtomicBool::new(true);
        request_shutdown(&stop, "SIGINT");
        assert!(stop.load(Ordering::SeqCst));
    }

    #[test]
    fn request_shutdown_emits_a_named_shutdown_breadcrumb() {
        use crate::test_support::capture;
        let logs = capture(|| {
            let stop = AtomicBool::new(false);
            request_shutdown(&stop, "SIGTERM");
        });
        assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
        assert!(
            logs.contains("studio_worker::runtime"),
            "expected runtime target, got: {logs}"
        );
        assert!(
            logs.contains("op=\"shutdown\""),
            "expected op field, got: {logs}"
        );
        assert!(
            logs.contains("signal=\"SIGTERM\""),
            "expected signal field, got: {logs}"
        );
    }

    #[tokio::test]
    async fn auto_update_tick_disabled_when_flag_off() {
        let cfg = Config {
            auto_update_enabled: false,
            ..Config::default()
        };
        let logs = Arc::new(Mutex::new(Vec::new()));
        let decision = auto_update_tick(&cfg, false, &logs).await;
        assert_eq!(decision, AutoUpdateDecision::Disabled);
    }

    #[tokio::test]
    async fn auto_update_tick_skipped_when_busy() {
        let cfg = Config {
            auto_update_enabled: true,
            ..Config::default()
        };
        let logs = Arc::new(Mutex::new(Vec::new()));
        let decision = auto_update_tick(&cfg, true, &logs).await;
        assert_eq!(decision, AutoUpdateDecision::SkippedBusy);
        let entries = logs.lock();
        assert!(entries.iter().any(|e| e.message.contains("busy on a job")));
    }

    #[tokio::test]
    async fn wait_with_stop_short_circuits_when_already_stopped() {
        let stop = Arc::new(AtomicBool::new(true));
        let start = std::time::Instant::now();
        wait_with_stop(Duration::from_secs(60), &stop, Duration::from_millis(10)).await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "an already-set stop must return without sleeping the full duration"
        );
    }

    #[tokio::test]
    async fn auto_updater_stops_promptly_during_idle_wait() {
        // A huge auto_update_tick means a non-cancellable idle sleep
        // would pin the JoinHandle — and thus `run_loops`' join() — for
        // the whole tick after stop is set, defeating graceful
        // shutdown.  The stop-aware wait must let the task finish well
        // inside the tick.
        let cfg = crate::config::shared(Config {
            auto_update_enabled: false,
            ..Config::default()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let schedule = LoopSchedule {
            auto_update_tick: Duration::from_secs(3600),
            shutdown_tick: Duration::from_millis(1),
        };
        let handle = spawn_auto_updater(cfg, stop.clone(), logs, busy, schedule);
        // Let the loop reach its idle wait, then request shutdown.
        tokio::time::sleep(Duration::from_millis(10)).await;
        stop.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_millis(250), handle)
            .await
            .expect("auto-updater did not observe stop promptly")
            .expect("auto-updater task panicked");
    }
}
