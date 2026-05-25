//! Long-running WebSocket session that owns the worker's lifecycle.
//!
//! Replaces the four polling loops (`spawn_heartbeat`, `spawn_claim_loop`,
//! `spawn_log_shipper`, plus the implicit completion path) with a single
//! `spawn_ws_session` coordinator + a small handful of helper tasks that
//! all push frames through a shared `WsSender`.
//!
//! Reconnect policy: on a transport error or non-auth close, back off
//! `BASE_BACKOFF_MS * 2^attempt` and try again, up to
//! `cfg.ws_reconnect_attempts`.  Out of retries \u2192 return `Err` and the
//! systemd / launchd unit restarts the binary.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::SharedConfig;
use crate::engine::Engine;
use crate::http::ApiClient;
use crate::runtime::{build_capabilities, is_unsupported_kind, prompt_for, push_log};
use crate::types::{LogEntry, TaskResult, WorkerCapabilities};
use crate::ws::client::{connect, WsClientError, WsSender};
use crate::ws::types::{HelloFrame, JobOfferClaim, WorkerInbound, WorkerOutbound};

/// Tracing target used for every event emitted by the session.
const TRACE_TARGET: &str = "studio_worker::ws::session";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const SHUTDOWN_TICK: Duration = Duration::from_millis(250);
const BASE_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 30_000;
const DEFAULT_RECONNECT_ATTEMPTS: u32 = 5;

/// Outcome of a single session attempt.  The reconnect loop decides
/// whether to back off + retry based on the variant.
#[derive(Debug)]
pub enum SessionOutcome {
    /// Caller requested shutdown; do not reconnect.
    Stopped,
    /// Lost the connection unexpectedly; reconnect after backoff.
    Disconnected,
    /// Server rejected auth; do not reconnect.
    AuthFailed(String),
    /// Server sent a fatal error frame; do not reconnect.
    Fatal(String),
}

/// Tunables for the session loop \u2014 dialed down in tests.
#[derive(Debug, Clone, Copy)]
pub struct SessionSchedule {
    pub heartbeat: Duration,
    pub log_flush: Duration,
    pub shutdown_tick: Duration,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for SessionSchedule {
    fn default() -> Self {
        Self {
            heartbeat: HEARTBEAT_INTERVAL,
            log_flush: LOG_FLUSH_INTERVAL,
            shutdown_tick: SHUTDOWN_TICK,
            base_backoff_ms: BASE_BACKOFF_MS,
            max_backoff_ms: MAX_BACKOFF_MS,
        }
    }
}

impl SessionSchedule {
    pub fn fast_for_tests() -> Self {
        Self {
            heartbeat: Duration::from_millis(5),
            log_flush: Duration::from_millis(5),
            shutdown_tick: Duration::from_millis(5),
            base_backoff_ms: 1,
            max_backoff_ms: 10,
        }
    }
}

/// Top-level driver: connect, run a session, reconnect on disconnect,
/// give up after `cfg.ws_reconnect_attempts` failures.
pub async fn spawn_ws_session(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    schedule: SessionSchedule,
) -> Result<()> {
    let max_attempts = {
        let guard = cfg.lock();
        guard
            .ws_reconnect_attempts
            .unwrap_or(DEFAULT_RECONNECT_ATTEMPTS)
    };

    let mut attempt: u32 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match run_one_session(&cfg, &stop, &logs, &busy, schedule).await {
            Ok(SessionOutcome::Stopped) => return Ok(()),
            Ok(SessionOutcome::AuthFailed(reason)) => {
                push_log(
                    &logs,
                    "error",
                    "ws",
                    &format!("auth failed: {reason}. Re-register the worker."),
                    None,
                );
                return Err(anyhow!("ws auth failed: {reason}"));
            }
            Ok(SessionOutcome::Fatal(reason)) => {
                push_log(&logs, "error", "ws", &format!("fatal: {reason}"), None);
                return Err(anyhow!("ws fatal: {reason}"));
            }
            Ok(SessionOutcome::Disconnected) | Err(_) => {
                attempt += 1;
                if max_attempts > 0 && attempt > max_attempts {
                    push_log(
                        &logs,
                        "error",
                        "ws",
                        &format!("giving up after {attempt} reconnect attempts"),
                        None,
                    );
                    return Err(anyhow!("ws reconnect cap reached"));
                }
                let backoff = backoff_for(attempt, schedule);
                push_log(
                    &logs,
                    "warn",
                    "ws",
                    &format!(
                        "disconnected; reconnect attempt {attempt} in {}ms",
                        backoff.as_millis()
                    ),
                    None,
                );
                wait_with_stop(backoff, &stop, schedule.shutdown_tick).await;
            }
        }
    }
}

/// One end-to-end session attempt: connect, hello, run until shutdown
/// or disconnect.
async fn run_one_session(
    cfg: &SharedConfig,
    stop: &Arc<AtomicBool>,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    busy: &Arc<AtomicBool>,
    schedule: SessionSchedule,
) -> Result<SessionOutcome> {
    let (api_base_url, worker_id, auth_token) = {
        let guard = cfg.lock();
        (
            guard.api_base_url.clone(),
            guard.worker_id.clone().unwrap_or_default(),
            guard.auth_token.clone().unwrap_or_default(),
        )
    };
    if worker_id.is_empty() || auth_token.is_empty() {
        return Ok(SessionOutcome::Fatal(
            "worker_id or auth_token missing; run register".to_string(),
        ));
    }

    push_log(
        logs,
        "info",
        "ws",
        &format!("connecting to {api_base_url}"),
        None,
    );
    let client = match connect(&api_base_url, &worker_id, &auth_token).await {
        Ok(c) => c,
        Err(WsClientError::AuthFailed { reason }) => {
            return Ok(SessionOutcome::AuthFailed(reason));
        }
        Err(e) => {
            push_log(logs, "warn", "ws", &format!("connect failed: {e}"), None);
            return Ok(SessionOutcome::Disconnected);
        }
    };
    let (sender, receiver) = client.split();

    // Send hello with the current capabilities.
    let engine = crate::engine::build(&cfg.lock())?;
    let capabilities = build_capabilities(&cfg.lock(), &*engine);
    sender
        .send(&WorkerInbound::Hello(HelloFrame {
            auth_token: auth_token.clone(),
            capabilities: capabilities.clone(),
        }))
        .await
        .map_err(|e| anyhow!("hello send failed: {e}"))?;
    info!(target: TRACE_TARGET, worker_id = %worker_id, "hello sent");

    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();

    // Reader task: pump frames into the event channel.
    let reader = spawn_reader(receiver, event_tx.clone());

    // Heartbeat task.
    let heartbeat = spawn_heartbeat_pump(
        cfg.clone(),
        sender.clone(),
        stop.clone(),
        busy.clone(),
        schedule,
    );

    // Log shipper task.
    let log_shipper = spawn_log_shipper_pump(sender.clone(), logs.clone(), stop.clone(), schedule);

    // Shutdown observer: ticks until stop flag is set, then drops the channel.
    let shutdown_observer = spawn_shutdown_observer(stop.clone(), event_tx.clone(), schedule);
    drop(event_tx);

    let engine_arc: Arc<dyn Engine> = engine.into();
    let ctx = SessionContext {
        sender: sender.clone(),
        engine: engine_arc,
        logs: logs.clone(),
        busy: busy.clone(),
        api_base_url: api_base_url.clone(),
        worker_id: worker_id.clone(),
        auth_token: auth_token.clone(),
    };
    let outcome = run_dispatch_loop(ctx, event_rx).await;

    // Best-effort graceful close + task drain.
    let _ = sender.close(1000, "session ended").await;
    let _ = reader.await;
    let _ = heartbeat.await;
    let _ = log_shipper.await;
    let _ = shutdown_observer.await;
    Ok(outcome)
}

/// All the events the dispatch loop reacts to.
#[derive(Debug)]
enum SessionEvent {
    /// Frame arrived from the server.
    Frame(WorkerOutbound),
    /// Engine task finished (success or fail already reported).
    Stopped,
    /// Reader hit EOF / error.
    Disconnected(WsClientError),
}

/// Bundle of immutable per-session settings the dispatcher passes
/// around — keeps clippy's `too_many_arguments` lint happy.
struct SessionContext {
    sender: WsSender,
    engine: Arc<dyn Engine>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    api_base_url: String,
    worker_id: String,
    auth_token: String,
}

async fn run_dispatch_loop(
    ctx: SessionContext,
    mut event_rx: mpsc::UnboundedReceiver<SessionEvent>,
) -> SessionOutcome {
    while let Some(event) = event_rx.recv().await {
        match event {
            SessionEvent::Disconnected(WsClientError::AuthFailed { reason }) => {
                return SessionOutcome::AuthFailed(reason);
            }
            SessionEvent::Disconnected(_) => return SessionOutcome::Disconnected,
            SessionEvent::Stopped => return SessionOutcome::Stopped,
            SessionEvent::Frame(frame) => match frame {
                WorkerOutbound::Welcome { worker_id: wid, .. } => {
                    push_log(
                        &ctx.logs,
                        "info",
                        "ws",
                        &format!("server welcomed {wid}"),
                        None,
                    );
                }
                WorkerOutbound::Offer { claim } => {
                    handle_offer(&ctx, claim);
                }
                WorkerOutbound::Error { code, message } => {
                    push_log(
                        &ctx.logs,
                        "error",
                        "ws",
                        &format!("server error {code:?}: {message}"),
                        None,
                    );
                    return match code {
                        crate::ws::types::WorkerErrorCode::AuthFailed => {
                            SessionOutcome::AuthFailed(message)
                        }
                        _ => SessionOutcome::Fatal(message),
                    };
                }
                WorkerOutbound::HeartbeatAck
                | WorkerOutbound::CompleteAck { .. }
                | WorkerOutbound::FailAck { .. } => {
                    // Acks are best-effort; ignore.
                }
            },
        }
    }
    SessionOutcome::Disconnected
}

fn handle_offer(ctx: &SessionContext, claim: JobOfferClaim) {
    let job_id = claim.job_id.clone();
    push_log(
        &ctx.logs,
        "info",
        "ws",
        &format!(
            "offer received {job_id} model={} vram={}",
            claim.model, claim.vram_gb_estimate
        ),
        Some(job_id.clone()),
    );
    // Send accept first so the server marks the offer consumed.
    let sender_for_accept = ctx.sender.clone();
    let job_id_for_accept = job_id.clone();
    tokio::spawn(async move {
        let _ = sender_for_accept
            .send(&WorkerInbound::Accept {
                job_id: job_id_for_accept,
            })
            .await;
    });

    let job = claim.into_job_claim();
    let busy_flag = ctx.busy.clone();
    busy_flag.store(true, Ordering::SeqCst);
    let logs_for_task = ctx.logs.clone();
    let sender_for_task = ctx.sender.clone();
    let engine_for_task = ctx.engine.clone();
    let api_base_url = ctx.api_base_url.clone();
    let worker_id = ctx.worker_id.clone();
    let auth_token = ctx.auth_token.clone();
    tokio::spawn(async move {
        run_offered_job(
            sender_for_task,
            engine_for_task,
            logs_for_task,
            api_base_url,
            worker_id,
            auth_token,
            job,
        )
        .await;
        busy_flag.store(false, Ordering::SeqCst);
    });
}

async fn run_offered_job(
    sender: WsSender,
    engine: Arc<dyn Engine>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    api_base_url: String,
    worker_id: String,
    auth_token: String,
    job: crate::types::JobClaim,
) {
    let task = job.resolved_task();
    let task_kind = task.kind();
    let prompt_for_log = prompt_for(&task);
    let start = std::time::Instant::now();
    let dispatch = tokio::task::spawn_blocking({
        let model = job.model.clone();
        let task_for_engine = task;
        let engine = engine.clone();
        move || -> Result<TaskResult> { engine.dispatch(&model, task_for_engine) }
    })
    .await;

    let job_id = job.job_id.clone();
    match dispatch {
        Ok(Ok(result)) => {
            push_log(
                &logs,
                "info",
                "ws",
                &format!("{} dispatched in {:?}", task_kind.as_str(), start.elapsed()),
                Some(job_id.clone()),
            );
            match result {
                TaskResult::Image { bytes, ext }
                | TaskResult::AudioTts { bytes, ext }
                | TaskResult::Video { bytes, ext } => {
                    // Binary outputs go via HTTP multipart \u2014 the only
                    // worker-side HTTP route that survives the migration.
                    let upload_result = tokio::task::spawn_blocking({
                        let api_base_url = api_base_url.clone();
                        let job_id = job_id.clone();
                        let auth_token = auth_token.clone();
                        let worker_id = worker_id.clone();
                        let prompt = prompt_for_log.clone();
                        move || -> Result<()> {
                            let api = ApiClient::new(api_base_url)?;
                            api.complete(&worker_id, &auth_token, &job_id, &ext, &prompt, bytes)
                        }
                    })
                    .await;
                    let msg = match upload_result {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(e.to_string()),
                        Err(e) => Some(format!("upload task panic: {e}")),
                    };
                    if let Some(msg) = msg {
                        push_log(&logs, "error", "ws", &msg, Some(job_id.clone()));
                        let _ = sender
                            .send(&WorkerInbound::Fail {
                                job_id: job_id.clone(),
                                error: msg,
                                retryable: true,
                            })
                            .await;
                    } else {
                        push_log(
                            &logs,
                            "info",
                            "ws",
                            "binary upload ok",
                            Some(job_id.clone()),
                        );
                        // Nudge the server so it offers the next job.
                        let _ = sender.send(&WorkerInbound::ReadyForMore).await;
                    }
                }
                TaskResult::Llm { json } | TaskResult::AudioStt { json } => {
                    let _ = sender
                        .send(&WorkerInbound::CompleteJson {
                            job_id: job_id.clone(),
                            result: json,
                            prompt: Some(prompt_for_log.clone()),
                        })
                        .await;
                }
            }
        }
        Ok(Err(e)) => {
            warn!(target: TRACE_TARGET, error = %e, "engine dispatch failed");
            push_log(
                &logs,
                "error",
                "ws",
                &format!("dispatch failed: {e}"),
                Some(job_id.clone()),
            );
            let _ = sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: !is_unsupported_kind(&e),
                })
                .await;
        }
        Err(e) => {
            push_log(
                &logs,
                "error",
                "ws",
                &format!("dispatch task panic: {e}"),
                Some(job_id.clone()),
            );
            let _ = sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: true,
                })
                .await;
        }
    }
}

fn spawn_reader(
    mut receiver: crate::ws::client::WsReceiver,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(Some(frame)) => {
                    if event_tx.send(SessionEvent::Frame(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ =
                        event_tx.send(SessionEvent::Disconnected(WsClientError::ConnectionClosed));
                    break;
                }
                Err(e) => {
                    let _ = event_tx.send(SessionEvent::Disconnected(e));
                    break;
                }
            }
        }
    })
}

fn spawn_heartbeat_pump(
    cfg: SharedConfig,
    sender: WsSender,
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    schedule: SessionSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(schedule.heartbeat);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let snapshot = build_heartbeat_snapshot(&cfg, &busy);
            if let Err(e) = sender
                .send(&WorkerInbound::Heartbeat {
                    capabilities: snapshot.capabilities,
                    current_job_id: snapshot.current_job_id,
                })
                .await
            {
                warn!(target: TRACE_TARGET, error = %e, "heartbeat send failed");
                break;
            }
        }
    })
}

struct HeartbeatSnapshot {
    capabilities: WorkerCapabilities,
    current_job_id: Option<String>,
}

fn build_heartbeat_snapshot(cfg: &SharedConfig, busy: &Arc<AtomicBool>) -> HeartbeatSnapshot {
    let engine = match crate::engine::build(&cfg.lock()) {
        Ok(e) => e,
        Err(_) => return placeholder_snapshot(),
    };
    let capabilities = build_capabilities(&cfg.lock(), &*engine);
    let current_job_id = if busy.load(Ordering::SeqCst) {
        Some("in-flight".to_string())
    } else {
        None
    };
    HeartbeatSnapshot {
        capabilities,
        current_job_id,
    }
}

fn placeholder_snapshot() -> HeartbeatSnapshot {
    HeartbeatSnapshot {
        capabilities: WorkerCapabilities {
            machine_name: String::new(),
            username: String::new(),
            agent_version: crate::AGENT_VERSION.to_string(),
            engine: "synthetic".to_string(),
            vram_total_gb: 0.0,
            vram_threshold_gb: 0.0,
            auto_enabled: false,
            auto_start: false,
            supported_models: vec![],
            task_kinds: vec![],
            supported_models_per_kind: Default::default(),
        },
        current_job_id: None,
    }
}

fn spawn_log_shipper_pump(
    sender: WsSender,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    stop: Arc<AtomicBool>,
    schedule: SessionSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(schedule.log_flush);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let batch = {
                let mut guard = logs.lock();
                if guard.is_empty() {
                    continue;
                }
                std::mem::take(&mut *guard)
            };
            if let Err(e) = sender
                .send(&WorkerInbound::LogBatch { entries: batch })
                .await
            {
                warn!(target: TRACE_TARGET, error = %e, "log batch send failed");
                break;
            }
        }
    })
}

fn spawn_shutdown_observer(
    stop: Arc<AtomicBool>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    schedule: SessionSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(schedule.shutdown_tick).await;
            if stop.load(Ordering::SeqCst) {
                let _ = event_tx.send(SessionEvent::Stopped);
                break;
            }
            if event_tx.is_closed() {
                break;
            }
        }
    })
}

async fn wait_with_stop(total: Duration, stop: &Arc<AtomicBool>, tick: Duration) {
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

fn backoff_for(attempt: u32, schedule: SessionSchedule) -> Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let raw_ms = schedule.base_backoff_ms.saturating_mul(factor);
    Duration::from_millis(raw_ms.min(schedule.max_backoff_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        let schedule = SessionSchedule {
            base_backoff_ms: 100,
            max_backoff_ms: 1_000,
            heartbeat: Duration::from_secs(1),
            log_flush: Duration::from_secs(1),
            shutdown_tick: Duration::from_secs(1),
        };
        assert_eq!(backoff_for(1, schedule), Duration::from_millis(100));
        assert_eq!(backoff_for(2, schedule), Duration::from_millis(200));
        assert_eq!(backoff_for(3, schedule), Duration::from_millis(400));
        assert_eq!(backoff_for(4, schedule), Duration::from_millis(800));
        // Capped.
        assert_eq!(backoff_for(5, schedule), Duration::from_millis(1_000));
        assert_eq!(backoff_for(10, schedule), Duration::from_millis(1_000));
    }

    #[test]
    fn placeholder_snapshot_has_no_current_job() {
        let snap = placeholder_snapshot();
        assert!(snap.current_job_id.is_none());
        assert_eq!(snap.capabilities.engine, "synthetic");
    }
}
