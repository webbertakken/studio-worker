//! Long-running auto-update task + one-shot CLI helpers.
//!
//! After the WS migration the runtime owns just two background
//! tasks: the WebSocket session (`ws::session::spawn_ws_session`,
//! which subsumes heartbeats, claim/accept/complete, fail, and log
//! shipping) and the auto-updater (`spawn_auto_updater`).  Per-tick
//! helpers from the old polling loops are gone.
use crate::{
    config::{self, Config, SharedConfig},
    engine::{self, Engine},
    http::ApiClient,
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
use tracing::info;

/// Tracing target for runtime-level events (startup, state mutations).
/// Stable so operators can filter with `RUST_LOG=studio_worker::runtime=debug`.
const TRACE_TARGET: &str = "studio_worker::runtime";

/// Maximum number of finished jobs kept in `WorkerObservers::recent_jobs`.
/// Older entries fall off the back of the ring.
pub const RECENT_JOBS_CAP: usize = 50;

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
/// Default WS heartbeat interval, re-exported here so the native UI
/// (and any other downstream readers) get a stable constant without
/// reaching into `ws::session`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Schedule for the auto-updater loop.  The WS session has its own
/// `SessionSchedule` (see `ws::session`).
#[derive(Debug, Clone, Copy)]
pub struct LoopSchedule {
    pub auto_update_tick: Duration,
}

impl Default for LoopSchedule {
    fn default() -> Self {
        Self {
            auto_update_tick: AUTO_UPDATE_TICK,
        }
    }
}

impl LoopSchedule {
    /// Schedule with 1 ms intervals — used by tests to exercise the
    /// loop wrappers without blocking.
    pub fn fast_for_tests() -> Self {
        Self {
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
    let observers = WorkerObservers::default();

    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop_clone.store(true, Ordering::SeqCst);
    });

    run_loops(cfg, stop, logs, busy, observers, LoopSchedule::default()).await
}

/// Spawn the WS session + auto-updater, wait for them.  Pulled out of
/// `run` so tests can drive with a different schedule.
pub async fn run_loops(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    observers: WorkerObservers,
    schedule: LoopSchedule,
) -> Result<()> {
    let session_schedule = crate::ws::session::SessionSchedule::default();
    let session = crate::ws::session::spawn_ws_session(
        cfg.clone(),
        stop.clone(),
        logs.clone(),
        busy.clone(),
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
}
