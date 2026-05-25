//! Long-running heartbeat + claim loop, log shipper, auto-update task,
//! and the one-shot CLI helpers.
//!
//! The four loops (`spawn_heartbeat`, `spawn_claim_loop`,
//! `spawn_log_shipper`, `spawn_auto_updater`) are thin `while !stop`
//! wrappers around the testable `*_tick` helpers below — those are the
//! places real per-iteration logic lives, and they're 100% unit-tested.
use crate::{
    config::{self, Config, SharedConfig},
    engine::{self, Engine},
    http::ApiClient,
    sys,
    types::*,
    update, AGENT_VERSION,
};
use anyhow::{anyhow, Result};
use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use std::{
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

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
pub const CLAIM_INTERVAL_IDLE: Duration = Duration::from_secs(2);
pub const CLAIM_INTERVAL_AFTER_NULL: Duration = Duration::from_secs(5);
pub const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
pub const AUTO_UPDATE_TICK: Duration = Duration::from_secs(60);

/// Schedule for the four background loops.  Tests dial these intervals
/// down to milliseconds so we can exercise the loop bodies quickly.
#[derive(Debug, Clone, Copy)]
pub struct LoopSchedule {
    pub heartbeat: Duration,
    pub claim_idle: Duration,
    pub claim_after_null: Duration,
    pub log_flush: Duration,
    pub auto_update_tick: Duration,
}

impl Default for LoopSchedule {
    fn default() -> Self {
        Self {
            heartbeat: HEARTBEAT_INTERVAL,
            claim_idle: CLAIM_INTERVAL_IDLE,
            claim_after_null: CLAIM_INTERVAL_AFTER_NULL,
            log_flush: LOG_FLUSH_INTERVAL,
            auto_update_tick: AUTO_UPDATE_TICK,
        }
    }
}

impl LoopSchedule {
    /// Schedule with 1 ms intervals — used by tests to exercise the
    /// loop wrappers without blocking.
    pub fn fast_for_tests() -> Self {
        Self {
            heartbeat: Duration::from_millis(1),
            claim_idle: Duration::from_millis(1),
            claim_after_null: Duration::from_millis(1),
            log_flush: Duration::from_millis(1),
            auto_update_tick: Duration::from_millis(1),
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot helpers used by the CLI subcommands
// ---------------------------------------------------------------------------

pub async fn register(
    config_path: Option<&str>,
    bootstrap_override: Option<String>,
    api_base_url: Option<String>,
) -> Result<()> {
    let (mut cfg, path) = config::load(config_path)?;
    if let Some(token) = bootstrap_override {
        cfg.bootstrap_token = token;
    }
    if let Some(url) = api_base_url {
        cfg.api_base_url = url;
    }
    let engine = engine::build(&cfg)?;
    let cap = build_capabilities(&cfg, &*engine);
    let api_for_diag = cfg.api_base_url.clone();
    let response = tokio::task::spawn_blocking({
        let api_base_url = cfg.api_base_url.clone();
        let bootstrap = cfg.bootstrap_token.clone();
        let worker_id = cfg.worker_id.clone();
        let cap = cap.clone();
        move || -> Result<RegisterResponse> {
            let api = ApiClient::new(api_base_url)?;
            api.register(&bootstrap, cap, worker_id)
        }
    })
    .await?
    .map_err(|e| friendly_register_error(e, &api_for_diag))?;
    cfg.worker_id = Some(response.worker_id.clone());
    cfg.auth_token = Some(response.auth_token);
    config::save(&cfg, &path)?;
    info!(
        worker_id = %response.worker_id,
        api = %cfg.api_base_url,
        "registered with studio API"
    );
    Ok(())
}

/// Wrap network/HTTP errors from register() with a hint that points the
/// operator at `--api-base-url` and the right secret.  Saves people from
/// hitting the default `http://localhost:9790` and wondering what happened.
fn friendly_register_error(err: anyhow::Error, api_base_url: &str) -> anyhow::Error {
    // Walk the full error chain so we catch the cause inside
    // reqwest/hyper, not just the top-level wrap.
    let message = format!("{:#}", err);
    let is_connection_refused =
        message.contains("Connection refused") || message.contains("ConnectionRefused");
    if is_connection_refused {
        anyhow!(
            "could not reach the studio API at {api_base_url}: {message}\n\
             \n\
             Hint: pass --api-base-url <URL> on the register command, e.g.\n\
               studio-worker register \\\n\
                 --bootstrap-token <TOKEN> \\\n\
                 --api-base-url https://studio.example.com\n\
             \n\
             The bootstrap token is the WORKER_BOOTSTRAP_TOKEN wrangler secret\n\
             on the studio side (for local dev the default is `dev-bootstrap-token`)."
        )
    } else if message.contains("401") || message.contains("403") {
        anyhow!(
            "the studio API rejected our bootstrap token: {message}\n\
             \n\
             Check that --bootstrap-token matches the WORKER_BOOTSTRAP_TOKEN\n\
             secret on the studio side."
        )
    } else {
        err
    }
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
    let _ = writeln!(
        out,
        "worker_id:          {}",
        cfg.worker_id.as_deref().unwrap_or("(not registered)")
    );
    let _ = writeln!(out, "engine:             {}", cfg.engine);
    let _ = writeln!(out, "vram_threshold_gb:  {}", cfg.vram_threshold_gb);
    let _ = writeln!(out, "auto_enabled:       {}", cfg.auto_enabled);
    let _ = writeln!(out, "auto_start:         {}", cfg.auto_start);
    let _ = writeln!(out, "auto_update:        {}", cfg.auto_update_enabled);
    let _ = writeln!(
        out,
        "update_interval:    {}s",
        cfg.auto_update_interval_secs
    );
    out
}

pub fn set_enabled(config_path: Option<&str>, enabled: bool) -> Result<()> {
    let (mut cfg, path) = config::load(config_path)?;
    cfg.auto_enabled = enabled;
    config::save(&cfg, &path)?;
    info!(
        target: TRACE_TARGET,
        op = "set_enabled",
        auto_enabled = enabled,
        config_path = path.display().to_string(),
        "auto-claim flag persisted"
    );
    println!("auto_enabled = {enabled}");
    Ok(())
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
        engine = cfg.engine.as_str(),
        vram_threshold_gb = cfg.vram_threshold_gb,
        auto_enabled = cfg.auto_enabled,
        auto_update_enabled = cfg.auto_update_enabled,
        auto_update_interval_secs = cfg.auto_update_interval_secs,
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
    let (mut cfg, path) = config::load(config_path)?;
    log_startup_banner(&cfg, &path);
    if cfg.worker_id.is_none() || cfg.auth_token.is_none() {
        let engine = engine::build(&cfg)?;
        let cap = build_capabilities(&cfg, &*engine);
        let response = tokio::task::spawn_blocking({
            let api_base_url = cfg.api_base_url.clone();
            let bootstrap = cfg.bootstrap_token.clone();
            move || -> Result<RegisterResponse> {
                let api = ApiClient::new(api_base_url)?;
                api.register(&bootstrap, cap, None)
            }
        })
        .await??;
        cfg.worker_id = Some(response.worker_id);
        cfg.auth_token = Some(response.auth_token);
        config::save(&cfg, &path)?;
        info!(
            worker_id = %cfg.worker_id.as_deref().unwrap_or(""),
            "auto-registered on first run"
        );
    }

    let cfg = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));

    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop_clone.store(true, Ordering::SeqCst);
    });

    run_loops(cfg, stop, logs, busy, LoopSchedule::default()).await;
    Ok(())
}

/// Spawn the four loops with the supplied schedule and wait for them to
/// exit.  Pulled out of `run` so tests can drive the loop wrappers with
/// short intervals.
pub async fn run_loops(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    schedule: LoopSchedule,
) {
    let heartbeat = spawn_heartbeat(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
        schedule,
    );
    let claim = spawn_claim_loop(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
        schedule,
    );
    let log_shipper = spawn_log_shipper(cfg.clone(), stop.clone(), logs.clone(), schedule);
    let auto_updater = spawn_auto_updater(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
        schedule,
    );
    let _ = tokio::join!(heartbeat, claim, log_shipper, auto_updater);
}

// ---------------------------------------------------------------------------
// Per-tick helpers — pure async fns, easy to drive from unit tests.
// ---------------------------------------------------------------------------

/// Outcome of a single claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// We claimed and ran a job — back to the fast poll interval.
    RanJob,
    /// 204 from the API — nothing to do, back off briefly.
    NoJobs,
    /// HTTP / wiring error — back off briefly and try again.
    Error(String),
    /// Worker is currently `auto_enabled = false`; skip the claim entirely.
    Skipped,
}

/// Send one heartbeat.  Returns Ok regardless of whether the HTTP call
/// succeeded — failures are captured in the log buffer.
pub async fn heartbeat_tick(
    cfg: &Config,
    busy_now: bool,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
) -> Result<()> {
    let engine = match engine::build(cfg) {
        Ok(e) => e,
        Err(e) => {
            push_log(
                logs,
                "warn",
                "heartbeat",
                &format!("engine error: {e}"),
                None,
            );
            return Ok(());
        }
    };
    let cap = build_capabilities(cfg, &*engine);
    let token = cfg.auth_token.clone().unwrap_or_default();
    let worker_id = cfg.worker_id.clone().unwrap_or_default();
    let api_base_url = cfg.api_base_url.clone();
    let logs_for_task = logs.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let api = ApiClient::new(api_base_url)?;
        api.heartbeat(&worker_id, &token, cap, None)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            push_log(
                &logs_for_task,
                "warn",
                "heartbeat",
                &format!("heartbeat failed (busy={busy_now}): {e}"),
                None,
            );
        }
        Err(e) => {
            push_log(
                &logs_for_task,
                "warn",
                "heartbeat",
                &format!("heartbeat task panic: {e}"),
                None,
            );
        }
    }
    Ok(())
}

/// One claim attempt + (when a job is claimed) run-to-completion.
pub async fn claim_tick(
    cfg: &Config,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    busy: &Arc<AtomicBool>,
) -> ClaimOutcome {
    if !cfg.auto_enabled {
        return ClaimOutcome::Skipped;
    }
    let engine = match engine::build(cfg) {
        Ok(e) => e,
        Err(e) => {
            push_log(logs, "warn", "claim", &format!("engine error: {e}"), None);
            return ClaimOutcome::Error(e.to_string());
        }
    };
    let token = cfg.auth_token.clone().unwrap_or_default();
    let worker_id = cfg.worker_id.clone().unwrap_or_default();
    let api_base_url = cfg.api_base_url.clone();

    let claim_result = tokio::task::spawn_blocking({
        let token = token.clone();
        let worker_id = worker_id.clone();
        let api_base_url = api_base_url.clone();
        move || -> Result<(ApiClient, Option<JobClaim>)> {
            let api = ApiClient::new(api_base_url)?;
            let claim = api.claim(&worker_id, &token)?;
            Ok((api, claim))
        }
    })
    .await;

    match claim_result {
        Ok(Ok((api, Some(job)))) => {
            busy.store(true, Ordering::SeqCst);
            push_log(
                logs,
                "info",
                "claim",
                &format!(
                    "claimed job {} (model={}, vram={}GB)",
                    job.job_id, job.model, job.vram_gb_estimate
                ),
                Some(job.job_id.clone()),
            );
            // Drop the api on the blocking thread to avoid reqwest's
            // internal runtime dropping inside an async context.
            let logs_clone = logs.clone();
            let token_clone = token.clone();
            let worker_id_clone = worker_id.clone();
            let engine_handle = engine;
            let _ = tokio::task::spawn_blocking(move || {
                run_job(
                    &api,
                    &token_clone,
                    &worker_id_clone,
                    &*engine_handle,
                    &logs_clone,
                    job,
                );
            })
            .await;
            busy.store(false, Ordering::SeqCst);
            ClaimOutcome::RanJob
        }
        Ok(Ok((_api, None))) => ClaimOutcome::NoJobs,
        Ok(Err(e)) => {
            push_log(
                logs,
                "warn",
                "claim",
                &format!("claim request errored: {e}"),
                None,
            );
            ClaimOutcome::Error(e.to_string())
        }
        Err(e) => {
            push_log(
                logs,
                "warn",
                "claim",
                &format!("claim task panic: {e}"),
                None,
            );
            ClaimOutcome::Error(e.to_string())
        }
    }
}

/// Flush all buffered logs to the API.  Returns the number of entries
/// shipped (0 if no logs were buffered or the worker isn't registered).
pub async fn log_shipper_tick(cfg: &Config, logs: &Arc<Mutex<Vec<LogEntry>>>) -> usize {
    let token = cfg.auth_token.clone().unwrap_or_default();
    let worker_id = cfg.worker_id.clone().unwrap_or_default();
    if worker_id.is_empty() || token.is_empty() {
        // Drain the buffer anyway so it doesn't grow unbounded.
        logs.lock().clear();
        return 0;
    }
    let batch = {
        let mut guard = logs.lock();
        if guard.is_empty() {
            return 0;
        }
        LogBatch {
            entries: std::mem::take(&mut *guard),
        }
    };
    let count = batch.entries.len();
    let api_base_url = cfg.api_base_url.clone();
    let _ = tokio::task::spawn_blocking(move || -> Result<()> {
        let api = ApiClient::new(api_base_url)?;
        api.ship_logs(&worker_id, &token, batch)
    })
    .await;
    count
}

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

pub fn spawn_heartbeat(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    schedule: LoopSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(schedule.heartbeat).await;
            let snapshot = cfg.lock().clone();
            let busy_now = busy.load(Ordering::SeqCst);
            let _ = heartbeat_tick(&snapshot, busy_now, &logs).await;
        }
    })
}

pub fn spawn_claim_loop(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    schedule: LoopSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut next_delay = schedule.claim_idle;
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(next_delay).await;
            let snapshot = cfg.lock().clone();
            let outcome = claim_tick(&snapshot, &logs, &busy).await;
            next_delay = match outcome {
                ClaimOutcome::RanJob => schedule.claim_idle,
                _ => schedule.claim_after_null,
            };
        }
    })
}

/// Pure helper so the schedule decision is unit-testable.
pub fn next_delay_for(outcome: &ClaimOutcome) -> Duration {
    match outcome {
        ClaimOutcome::RanJob => CLAIM_INTERVAL_IDLE,
        ClaimOutcome::NoJobs | ClaimOutcome::Error(_) | ClaimOutcome::Skipped => {
            CLAIM_INTERVAL_AFTER_NULL
        }
    }
}

pub fn spawn_log_shipper(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    schedule: LoopSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(schedule.log_flush).await;
            let snapshot = cfg.lock().clone();
            let _ = log_shipper_tick(&snapshot, &logs).await;
        }
    })
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
            tokio::time::sleep(schedule.auto_update_tick).await;
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

// ---------------------------------------------------------------------------
// Job runner (shared by claim_tick) — pure-ish; HTTP via ApiClient.
// ---------------------------------------------------------------------------

fn run_job(
    api: &ApiClient,
    token: &str,
    worker_id: &str,
    engine: &dyn Engine,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    job: JobClaim,
) {
    let start = std::time::Instant::now();
    let task = job.resolved_task();
    let task_kind = task.kind();
    let prompt_for_log = prompt_for(&task);
    let result = engine.dispatch(&job.model, task);
    match result {
        Ok(task_result) => {
            push_log(
                logs,
                "info",
                "generate",
                &format!(
                    "{} task generated in {:?}",
                    task_kind.as_str(),
                    start.elapsed()
                ),
                Some(job.job_id.clone()),
            );
            let outcome = match task_result {
                TaskResult::Image { bytes, ext } => {
                    api.complete(worker_id, token, &job.job_id, &ext, &prompt_for_log, bytes)
                }
                TaskResult::AudioTts { bytes, ext } => {
                    api.complete(worker_id, token, &job.job_id, &ext, &prompt_for_log, bytes)
                }
                TaskResult::Video { bytes, ext } => {
                    api.complete(worker_id, token, &job.job_id, &ext, &prompt_for_log, bytes)
                }
                TaskResult::Llm { json } => {
                    api.complete_json(worker_id, token, &job.job_id, &prompt_for_log, &json)
                }
                TaskResult::AudioStt { json } => {
                    api.complete_json(worker_id, token, &job.job_id, &prompt_for_log, &json)
                }
            };
            if let Err(e) = outcome {
                push_log(
                    logs,
                    "error",
                    "complete",
                    &format!("complete failed: {e}"),
                    Some(job.job_id.clone()),
                );
            } else {
                push_log(
                    logs,
                    "info",
                    "complete",
                    "job uploaded",
                    Some(job.job_id.clone()),
                );
            }
        }
        Err(e) => {
            warn!("generate failed: {e:#}");
            push_log(
                logs,
                "error",
                "generate",
                &format!("generate failed: {e}"),
                Some(job.job_id.clone()),
            );
            let retryable = !is_unsupported_kind(&e);
            let _ = api.fail(worker_id, token, &job.job_id, &e.to_string(), retryable);
        }
    }
}

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
    let supported_models = if cfg.supported_models_override.is_empty() {
        supported_models
    } else {
        cfg.supported_models_override.clone()
    };

    WorkerCapabilities {
        machine_name: sys::machine_name(),
        username: sys::username(),
        agent_version: AGENT_VERSION.to_string(),
        engine: cfg.engine.clone(),
        vram_total_gb: vram,
        vram_threshold_gb: cfg.vram_threshold_gb,
        auto_enabled: cfg.auto_enabled,
        auto_start: cfg.auto_start,
        supported_models,
        task_kinds,
        supported_models_per_kind,
    }
}

pub fn push_log(
    logs: &Arc<Mutex<Vec<LogEntry>>>,
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
    logs.lock().push(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::SyntheticEngine;

    #[test]
    fn capabilities_advertises_all_synthetic_kinds() {
        let cfg = Config::default();
        let engine = SyntheticEngine::new(vec![]);
        let cap = build_capabilities(&cfg, &engine);
        assert_eq!(cap.engine, "synthetic");
        assert_eq!(cap.task_kinds.len(), TaskKind::ALL.len());
        for kind in TaskKind::ALL {
            assert!(cap.supported_models_per_kind.contains_key(&kind));
        }
    }

    #[test]
    fn capabilities_uses_override_for_legacy_flat_list() {
        let cfg = Config {
            supported_models_override: vec!["only-this".into()],
            ..Config::default()
        };
        let engine = SyntheticEngine::new(vec![]);
        let cap = build_capabilities(&cfg, &engine);
        assert_eq!(cap.supported_models, vec!["only-this".to_string()]);
    }

    #[test]
    fn prompt_for_extracts_per_kind() {
        let image = Task::Image(ImageParams {
            prompt: "a stone golem".into(),
            width: 512,
            height: 512,
            steps: 20,
            seed: None,
            ext: "webp".into(),
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
        });
        assert_eq!(prompt_for(&llm), "hi");

        let llm_empty = Task::Llm(LlmParams {
            messages: vec![],
            max_tokens: 1,
            temperature: 0.0,
        });
        assert_eq!(prompt_for(&llm_empty), "");

        let stt = Task::AudioStt(AudioSttParams {
            input_url: "https://example.com/clip.wav".into(),
            language: None,
        });
        assert_eq!(prompt_for(&stt), "https://example.com/clip.wav");

        let tts = Task::AudioTts(AudioTtsParams {
            text: "hi there".into(),
            voice: "v".into(),
            ext: "wav".into(),
        });
        assert_eq!(prompt_for(&tts), "hi there");

        let video = Task::Video(VideoParams {
            prompt: "a tiny dragon".into(),
            seconds: 1.0,
            width: 256,
            height: 256,
            ext: "mp4".into(),
        });
        assert_eq!(prompt_for(&video), "a tiny dragon");
    }

    #[test]
    fn is_unsupported_kind_matches_engine_message() {
        let err = anyhow!("gradio engine cannot serve llm tasks");
        assert!(is_unsupported_kind(&err));
        let other = anyhow!("network timeout");
        assert!(!is_unsupported_kind(&other));
    }

    #[test]
    fn next_delay_for_picks_idle_after_a_job() {
        assert_eq!(next_delay_for(&ClaimOutcome::RanJob), CLAIM_INTERVAL_IDLE);
    }

    #[test]
    fn next_delay_for_backs_off_when_no_jobs_or_errors() {
        assert_eq!(
            next_delay_for(&ClaimOutcome::NoJobs),
            CLAIM_INTERVAL_AFTER_NULL
        );
        assert_eq!(
            next_delay_for(&ClaimOutcome::Error("boom".into())),
            CLAIM_INTERVAL_AFTER_NULL
        );
        assert_eq!(
            next_delay_for(&ClaimOutcome::Skipped),
            CLAIM_INTERVAL_AFTER_NULL
        );
    }

    #[test]
    fn format_status_includes_every_field() {
        let cfg = Config::default();
        let out = format_status(&cfg, std::path::Path::new("/tmp/x.toml"));
        assert!(out.contains("config path:"));
        assert!(out.contains("api_base_url:"));
        assert!(out.contains("worker_id:"));
        assert!(out.contains("(not registered)"));
        assert!(out.contains("auto_update:"));
        assert!(out.contains("update_interval:"));
    }

    #[test]
    fn format_status_shows_worker_id_when_registered() {
        let cfg = Config {
            worker_id: Some("w-abc".into()),
            ..Config::default()
        };
        let out = format_status(&cfg, std::path::Path::new("/tmp/x.toml"));
        assert!(out.contains("w-abc"));
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

    fn cfg_pointing_at(api_base_url: String) -> Config {
        Config {
            api_base_url,
            worker_id: Some("w-test".into()),
            auth_token: Some("tok-test".into()),
            engine: "synthetic".into(),
            auto_enabled: true,
            auto_update_enabled: false,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn claim_tick_returns_skipped_when_auto_enabled_is_false() {
        let cfg = Config {
            auto_enabled: false,
            ..Config::default()
        };
        let logs = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let outcome = claim_tick(&cfg, &logs, &busy).await;
        assert_eq!(outcome, ClaimOutcome::Skipped);
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
    async fn log_shipper_tick_returns_zero_when_buffer_empty() {
        let cfg = cfg_pointing_at("http://unused.invalid".into());
        let logs = Arc::new(Mutex::new(Vec::new()));
        let n = log_shipper_tick(&cfg, &logs).await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn log_shipper_tick_returns_zero_when_unregistered() {
        let cfg = Config {
            worker_id: None,
            auth_token: None,
            ..cfg_pointing_at("http://unused.invalid".into())
        };
        let logs = Arc::new(Mutex::new(vec![LogEntry {
            ts: "ts".into(),
            level: "info".into(),
            category: "x".into(),
            message: "m".into(),
            job_id: None,
        }]));
        let n = log_shipper_tick(&cfg, &logs).await;
        assert_eq!(n, 0);
    }
}
