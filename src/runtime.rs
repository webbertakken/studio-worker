//! Long-running heartbeat + claim loop and the one-shot CLI helpers.
use crate::{
    config::{self, Config, SharedConfig},
    engine::{self, Engine},
    http::ApiClient,
    sys,
    types::*,
    AGENT_VERSION,
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use parking_lot::Mutex;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{info, warn};

// We re-use the system clock indirectly via chrono.
use chrono::SecondsFormat;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLAIM_INTERVAL_IDLE: Duration = Duration::from_secs(2);
const CLAIM_INTERVAL_AFTER_NULL: Duration = Duration::from_secs(5);
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

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
    let api = ApiClient::new(cfg.api_base_url.clone())?;
    let engine = engine::build(&cfg)?;
    let cap = build_capabilities(&cfg, &*engine);
    let response = tokio::task::spawn_blocking({
        let bootstrap = cfg.bootstrap_token.clone();
        let worker_id = cfg.worker_id.clone();
        let cap = cap.clone();
        move || api.register(&bootstrap, cap, worker_id)
    })
    .await??;
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

pub async fn status(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    println!("config path:        {}", path.display());
    println!("api_base_url:       {}", cfg.api_base_url);
    println!(
        "worker_id:          {}",
        cfg.worker_id.as_deref().unwrap_or("(not registered)")
    );
    println!("engine:             {}", cfg.engine);
    println!("vram_threshold_gb:  {}", cfg.vram_threshold_gb);
    println!("auto_enabled:       {}", cfg.auto_enabled);
    println!("auto_start:         {}", cfg.auto_start);
    Ok(())
}

pub fn set_enabled(config_path: Option<&str>, enabled: bool) -> Result<()> {
    let (mut cfg, path) = config::load(config_path)?;
    cfg.auto_enabled = enabled;
    config::save(&cfg, &path)?;
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
    println!("vram_threshold_gb = {gb}");
    Ok(())
}

pub fn show_config(config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = config::load(config_path)?;
    println!("# {}", path.display());
    print!("{}", toml::to_string_pretty(&cfg)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// Long-running run loop
// ---------------------------------------------------------------------------

pub async fn run(config_path: Option<&str>) -> Result<()> {
    let (mut cfg, path) = config::load(config_path)?;
    if cfg.worker_id.is_none() || cfg.auth_token.is_none() {
        // Auto-register on first run.
        let api = ApiClient::new(cfg.api_base_url.clone())?;
        let engine = engine::build(&cfg)?;
        let cap = build_capabilities(&cfg, &*engine);
        let response = tokio::task::spawn_blocking({
            let bootstrap = cfg.bootstrap_token.clone();
            move || api.register(&bootstrap, cap, None)
        })
        .await??;
        cfg.worker_id = Some(response.worker_id);
        cfg.auth_token = Some(response.auth_token);
        config::save(&cfg, &path)?;
        info!(worker_id = %cfg.worker_id.as_deref().unwrap_or(""), "auto-registered on first run");
    }

    let cfg = config::shared(cfg);
    let stop = Arc::new(AtomicBool::new(false));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));

    // Set up Ctrl-C handler so the run loop exits cleanly.
    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop_clone.store(true, Ordering::SeqCst);
    });

    let heartbeat = spawn_heartbeat(cfg.clone(), stop.clone(), logs.clone());
    let claim = spawn_claim_loop(cfg.clone(), stop.clone(), logs.clone());
    let log_shipper = spawn_log_shipper(cfg.clone(), stop.clone(), logs.clone());

    let _ = tokio::join!(heartbeat, claim, log_shipper);
    Ok(())
}

fn spawn_heartbeat(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            let snapshot = cfg.lock().clone();
            let api = match ApiClient::new(snapshot.api_base_url.clone()) {
                Ok(api) => api,
                Err(e) => {
                    push_log(
                        &logs,
                        "warn",
                        "heartbeat",
                        &format!("api client error: {e}"),
                        None,
                    );
                    continue;
                }
            };
            let engine = match engine::build(&snapshot) {
                Ok(e) => e,
                Err(e) => {
                    push_log(
                        &logs,
                        "warn",
                        "heartbeat",
                        &format!("engine error: {e}"),
                        None,
                    );
                    continue;
                }
            };
            let cap = build_capabilities(&snapshot, &*engine);
            let token = snapshot.auth_token.clone().unwrap_or_default();
            let worker_id = snapshot.worker_id.clone().unwrap_or_default();
            let logs_for_task = logs.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = api.heartbeat(&worker_id, &token, cap, None) {
                    push_log(
                        &logs_for_task,
                        "warn",
                        "heartbeat",
                        &format!("heartbeat failed: {e}"),
                        None,
                    );
                }
            })
            .await;
        }
    })
}

fn spawn_claim_loop(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut next_delay = CLAIM_INTERVAL_IDLE;
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(next_delay).await;
            let snapshot = cfg.lock().clone();
            if !snapshot.auto_enabled {
                next_delay = CLAIM_INTERVAL_AFTER_NULL;
                continue;
            }
            let api = match ApiClient::new(snapshot.api_base_url.clone()) {
                Ok(api) => api,
                Err(e) => {
                    push_log(
                        &logs,
                        "warn",
                        "claim",
                        &format!("api client error: {e}"),
                        None,
                    );
                    next_delay = CLAIM_INTERVAL_AFTER_NULL;
                    continue;
                }
            };
            let engine = match engine::build(&snapshot) {
                Ok(e) => e,
                Err(e) => {
                    push_log(&logs, "warn", "claim", &format!("engine error: {e}"), None);
                    next_delay = CLAIM_INTERVAL_AFTER_NULL;
                    continue;
                }
            };
            let token = snapshot.auth_token.clone().unwrap_or_default();
            let worker_id = snapshot.worker_id.clone().unwrap_or_default();

            // Try to claim.
            let claim_result = tokio::task::spawn_blocking({
                let api_ref = ApiClient::new(snapshot.api_base_url.clone()).unwrap();
                let token = token.clone();
                let worker_id = worker_id.clone();
                move || api_ref.claim(&worker_id, &token)
            })
            .await
            .ok()
            .and_then(|r| r.ok());

            match claim_result {
                Some(Some(job)) => {
                    push_log(
                        &logs,
                        "info",
                        "claim",
                        &format!(
                            "claimed job {} (model={}, vram={}GB)",
                            job.job_id, job.model, job.vram_gb_estimate
                        ),
                        Some(job.job_id.clone()),
                    );
                    run_job(&api, &token, &worker_id, &*engine, &logs, job);
                    next_delay = CLAIM_INTERVAL_IDLE;
                }
                Some(None) => {
                    next_delay = CLAIM_INTERVAL_AFTER_NULL;
                }
                None => {
                    push_log(&logs, "warn", "claim", "claim request errored", None);
                    next_delay = CLAIM_INTERVAL_AFTER_NULL;
                }
            }
            let _ = engine; // keep alive until end of block
        }
    })
}

fn run_job(
    api: &ApiClient,
    token: &str,
    worker_id: &str,
    engine: &dyn Engine,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    job: JobClaim,
) {
    let start = std::time::Instant::now();
    let result = engine.generate(&job.prompt, &job.model, &job.ext);
    match result {
        Ok(bytes) => {
            push_log(
                logs,
                "info",
                "generate",
                &format!("rendered {} bytes in {:?}", bytes.len(), start.elapsed()),
                Some(job.job_id.clone()),
            );
            if let Err(e) =
                api.complete(worker_id, token, &job.job_id, &job.ext, &job.prompt, bytes)
            {
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
            let _ = api.fail(worker_id, token, &job.job_id, &e.to_string(), true);
        }
    }
}

fn spawn_log_shipper(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::SeqCst) {
            tokio::time::sleep(LOG_FLUSH_INTERVAL).await;
            let batch = {
                let mut guard = logs.lock();
                if guard.is_empty() {
                    continue;
                }
                LogBatch {
                    entries: std::mem::take(&mut *guard),
                }
            };
            let snapshot = cfg.lock().clone();
            let api = match ApiClient::new(snapshot.api_base_url.clone()) {
                Ok(api) => api,
                Err(e) => {
                    eprintln!("[studio-worker] log shipper api error: {e:#}");
                    continue;
                }
            };
            let token = snapshot.auth_token.clone().unwrap_or_default();
            let worker_id = snapshot.worker_id.clone().unwrap_or_default();
            if worker_id.is_empty() || token.is_empty() {
                continue;
            }
            let _ =
                tokio::task::spawn_blocking(move || api.ship_logs(&worker_id, &token, batch)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_capabilities(cfg: &Config, engine: &dyn Engine) -> WorkerCapabilities {
    let vram = sys::detect_vram_gb().unwrap_or(0.0);
    let supported = if cfg.supported_models_override.is_empty() {
        engine.supported_models()
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
        supported_models: supported,
    }
}

fn push_log(
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
    fn capabilities_reports_engine_models() {
        let cfg = Config::default();
        let engine = SyntheticEngine::new(vec![]);
        let cap = build_capabilities(&cfg, &engine);
        assert_eq!(cap.engine, "synthetic");
        assert!(cap.supported_models.contains(&"synthetic".to_string()));
    }

    #[test]
    fn capabilities_uses_override() {
        let cfg = Config {
            supported_models_override: vec!["only-this".into()],
            ..Config::default()
        };
        let engine = SyntheticEngine::new(vec![]);
        let cap = build_capabilities(&cfg, &engine);
        assert_eq!(cap.supported_models, vec!["only-this".to_string()]);
    }
}
