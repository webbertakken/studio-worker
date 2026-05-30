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
use crate::runtime::{
    is_unsupported_kind, prompt_for, push_log_with_observers, record_recent_job, truncate_prompt,
    wait_with_stop, CurrentJob, JobOutcome, RecentJob, WorkerObservers,
};
use crate::types::{LogEntry, TaskResult, WorkerCapabilities};
use crate::ws::client::{connect, WsClientError, WsResult, WsSender};
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
///
/// `paused` is a runtime-only flag (not persisted to `Config`).  When
/// true, the heartbeat reports `autoEnabled = false` and incoming
/// offers are rejected, so the studio stops sending new jobs.  In-
/// flight work is allowed to finish.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn spawn_ws_session(
    cfg: SharedConfig,
    stop: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    busy: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    observers: WorkerObservers,
    schedule: SessionSchedule,
) -> Result<()> {
    let max_attempts = {
        let guard = cfg.lock();
        guard
            .ws_reconnect_attempts
            .unwrap_or(DEFAULT_RECONNECT_ATTEMPTS)
    };

    let mut attempt: u32 = 0;
    let mut waiting_for_creds_logged = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Credentials may not exist yet (first launch — the
        // auto-register loop is racing to populate them).  Poll the
        // shared config until both `worker_id` and `auth_token` show
        // up, instead of failing the whole session loop.  This is
        // what lets the UI's parallel auto-register + WS flow work.
        if !has_credentials(&cfg) {
            if !waiting_for_creds_logged {
                push_log_with_observers(
                    &logs,
                    Some(&observers),
                    "info",
                    "ws",
                    "waiting for operator approval before opening the session",
                    None,
                );
                waiting_for_creds_logged = true;
            }
            wait_with_stop(Duration::from_secs(1), &stop, schedule.shutdown_tick).await;
            continue;
        }
        waiting_for_creds_logged = false;

        match run_one_session(&cfg, &stop, &logs, &busy, &paused, &observers, schedule).await {
            Ok(SessionOutcome::Stopped) => return Ok(()),
            Ok(SessionOutcome::AuthFailed(reason)) => {
                push_log_with_observers(
                    &logs,
                    Some(&observers),
                    "error",
                    "ws",
                    &format!("auth failed: {reason}. Re-register the worker."),
                    None,
                );
                return Err(anyhow!("ws auth failed: {reason}"));
            }
            Ok(SessionOutcome::Fatal(reason)) => {
                push_log_with_observers(
                    &logs,
                    Some(&observers),
                    "error",
                    "ws",
                    &format!("fatal: {reason}"),
                    None,
                );
                return Err(anyhow!("ws fatal: {reason}"));
            }
            Ok(SessionOutcome::Disconnected) | Err(_) => {
                attempt += 1;
                if max_attempts > 0 && attempt > max_attempts {
                    push_log_with_observers(
                        &logs,
                        Some(&observers),
                        "error",
                        "ws",
                        &format!("giving up after {attempt} reconnect attempts"),
                        None,
                    );
                    return Err(anyhow!("ws reconnect cap reached"));
                }
                let backoff = backoff_for(attempt, schedule);
                push_log_with_observers(
                    &logs,
                    Some(&observers),
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

/// Outcome of waiting for the server's Welcome (or an error) right
/// after sending Hello.  Drives the precondition gate that keeps the
/// heartbeat / log-shipper pumps from racing the studio's async auth
/// flow.
enum WelcomeOutcome {
    Welcomed,
    AuthFailed(String),
    Fatal(String),
    Disconnected,
}

/// Pull events from the reader until we see a Welcome (success) or an
/// Error / Disconnect (failure).  Any acks / offers that arrive
/// before the Welcome are pushed into the logs and discarded — the
/// studio shouldn't be sending them at this stage, but if it does,
/// the dispatch loop will pick the next ones up.
#[cfg_attr(coverage_nightly, coverage(off))]
async fn wait_for_welcome(
    event_rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    observers: &WorkerObservers,
) -> WelcomeOutcome {
    while let Some(event) = event_rx.recv().await {
        match event {
            SessionEvent::Frame(WorkerOutbound::Welcome { worker_id: wid, .. }) => {
                push_log_with_observers(
                    logs,
                    Some(observers),
                    "info",
                    "ws",
                    &format!("server welcomed {wid}"),
                    None,
                );
                return WelcomeOutcome::Welcomed;
            }
            SessionEvent::Frame(WorkerOutbound::Error { code, message }) => {
                push_log_with_observers(
                    logs,
                    Some(observers),
                    "error",
                    "ws",
                    &format!("server error before welcome {code:?}: {message}"),
                    None,
                );
                return match code {
                    crate::ws::types::WorkerErrorCode::AuthFailed => {
                        WelcomeOutcome::AuthFailed(message)
                    }
                    _ => WelcomeOutcome::Fatal(message),
                };
            }
            SessionEvent::Frame(other) => {
                push_log_with_observers(
                    logs,
                    Some(observers),
                    "warn",
                    "ws",
                    &format!("server sent unexpected frame before welcome: {other:?}"),
                    None,
                );
                // Keep waiting — maybe the next frame is Welcome.
            }
            SessionEvent::Disconnected(WsClientError::AuthFailed { reason }) => {
                return WelcomeOutcome::AuthFailed(reason);
            }
            SessionEvent::Disconnected(_) => return WelcomeOutcome::Disconnected,
            SessionEvent::Stopped => return WelcomeOutcome::Disconnected,
        }
    }
    WelcomeOutcome::Disconnected
}

/// True iff the shared config has both `worker_id` and `auth_token`
/// populated.  The auto-register flow writes them through on
/// approval.
fn has_credentials(cfg: &SharedConfig) -> bool {
    let guard = cfg.lock();
    guard
        .worker_id
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
        && guard
            .auth_token
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
}

/// One end-to-end session attempt: connect, hello, run until shutdown
/// or disconnect.
#[cfg_attr(coverage_nightly, coverage(off))]
async fn run_one_session(
    cfg: &SharedConfig,
    stop: &Arc<AtomicBool>,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    busy: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    observers: &WorkerObservers,
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

    push_log_with_observers(
        logs,
        Some(observers),
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
            push_log_with_observers(
                logs,
                Some(observers),
                "warn",
                "ws",
                &format!("connect failed: {e}"),
                None,
            );
            return Ok(SessionOutcome::Disconnected);
        }
    };
    let (sender, receiver) = client.split();

    // Send hello with the current capabilities.
    let engine = crate::engine::build(&cfg.lock())?;
    let capabilities = crate::runtime::build_capabilities_with(
        &cfg.lock(),
        &*engine,
        !paused.load(Ordering::SeqCst),
    );
    // Record exactly what we're about to advertise so the worker's logs
    // (and the studio's shipped-log view) show the offered kinds /
    // models / VRAM budget — otherwise the handshake is opaque and
    // "why won't it claim X jobs" can't be answered from the logs.
    push_log_with_observers(
        logs,
        Some(observers),
        "info",
        "ws",
        &crate::runtime::summarize_capabilities(&capabilities),
        None,
    );
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

    // Wait for the server's `Welcome` (or an error) before starting
    // the heartbeat / log-shipper pumps.  Without this gate, the
    // first heartbeat fires immediately (tokio `interval()` returns
    // at t=0) and races the studio's async Hello-auth flow: a
    // heartbeat arriving while the session is still marked
    // `authenticated: false` server-side gets rejected with
    // `protocol_violation: session not authenticated`, killing the
    // session.
    let mut event_rx = event_rx;
    match wait_for_welcome(&mut event_rx, logs, observers).await {
        WelcomeOutcome::Welcomed => {}
        WelcomeOutcome::AuthFailed(reason) => {
            let _ = sender.close(1000, "auth failed").await;
            let _ = reader.await;
            return Ok(SessionOutcome::AuthFailed(reason));
        }
        WelcomeOutcome::Fatal(reason) => {
            let _ = sender.close(1000, "protocol violation").await;
            let _ = reader.await;
            return Ok(SessionOutcome::Fatal(reason));
        }
        WelcomeOutcome::Disconnected => {
            let _ = reader.await;
            return Ok(SessionOutcome::Disconnected);
        }
    }

    // Heartbeat task.  Reuse the engine we already built for the
    // Hello frame instead of rebuilding it on every heartbeat —
    // rebuilding fires every engine's `try_new` / registration log
    // every 5s, floods the logs, and (for real engines like sd-cpp)
    // walks the disk to re-check model presence.
    let capabilities_for_heartbeat = capabilities.clone();
    let heartbeat = spawn_heartbeat_pump(
        capabilities_for_heartbeat,
        sender.clone(),
        stop.clone(),
        busy.clone(),
        paused.clone(),
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
        paused: paused.clone(),
        observers: observers.clone(),
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
    paused: Arc<AtomicBool>,
    observers: WorkerObservers,
    api_base_url: String,
    worker_id: String,
    auth_token: String,
}

#[cfg_attr(coverage_nightly, coverage(off))]
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
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        &format!("server welcomed {wid}"),
                        None,
                    );
                }
                WorkerOutbound::Offer { claim } => {
                    handle_offer(&ctx, *claim);
                }
                WorkerOutbound::Error { code, message } => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
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

#[cfg_attr(coverage_nightly, coverage(off))]
fn handle_offer(ctx: &SessionContext, claim: JobOfferClaim) {
    let job_id = claim.job_id.clone();
    push_log_with_observers(
        &ctx.logs,
        Some(&ctx.observers),
        "info",
        "ws",
        &format!(
            "offer received {job_id} model={} vram={}",
            claim.model, claim.vram_gb_estimate
        ),
        Some(job_id.clone()),
    );
    // Operator pressed Pause: reject the offer so the studio retries
    // on a different worker (or requeues until we resume).  No engine
    // dispatch, no busy flag flip.
    if ctx.paused.load(Ordering::SeqCst) {
        push_log_with_observers(
            &ctx.logs,
            Some(&ctx.observers),
            "info",
            "ws",
            &format!("rejecting offer {job_id}: worker is paused"),
            Some(job_id.clone()),
        );
        let sender_for_reject = ctx.sender.clone();
        let job_id_for_reject = job_id.clone();
        let logs_for_reject = ctx.logs.clone();
        let observers_for_reject = ctx.observers.clone();
        tokio::spawn(async move {
            let result = sender_for_reject
                .send(&WorkerInbound::Reject {
                    job_id: job_id_for_reject.clone(),
                    reason: "worker paused by operator".to_string(),
                })
                .await;
            if let Some((level, message)) =
                offer_response_breadcrumb("reject", &job_id_for_reject, &result)
            {
                push_log_with_observers(
                    &logs_for_reject,
                    Some(&observers_for_reject),
                    level,
                    "ws",
                    &message,
                    Some(job_id_for_reject),
                );
            }
        });
        return;
    }
    // Send accept first so the server marks the offer consumed.
    let sender_for_accept = ctx.sender.clone();
    let job_id_for_accept = job_id.clone();
    let logs_for_accept = ctx.logs.clone();
    let observers_for_accept = ctx.observers.clone();
    tokio::spawn(async move {
        let result = sender_for_accept
            .send(&WorkerInbound::Accept {
                job_id: job_id_for_accept.clone(),
            })
            .await;
        if let Some((level, message)) =
            offer_response_breadcrumb("accept", &job_id_for_accept, &result)
        {
            push_log_with_observers(
                &logs_for_accept,
                Some(&observers_for_accept),
                level,
                "ws",
                &message,
                Some(job_id_for_accept),
            );
        }
    });

    let job = claim.into_job_claim();
    let task_kind = job.task.kind();
    // The FULL prompt goes back to the studio (and to the engine).
    // The bounded preview (`truncate_prompt`) is only for the UI's
    // Jobs tab so the in-memory observer ring stays small even when
    // LLM prompts are huge.  Mixing the two used to send the
    // truncated 200-char preview as the `prompt` form field on the
    // multipart `/complete`, which the studio then persisted onto the
    // row — mangling every operator-facing prompt in the DB.
    let full_prompt = prompt_for(&job.task);
    let prompt_preview = truncate_prompt(&full_prompt);
    let started_at = chrono::Utc::now();

    // Surface the job to the UI's Jobs tab — bounded preview only.
    *ctx.observers.current_job.lock() = Some(CurrentJob {
        job_id: job_id.clone(),
        kind: task_kind,
        model: job.model.clone(),
        prompt: prompt_preview.clone(),
        started_at,
    });

    let busy_flag = ctx.busy.clone();
    busy_flag.store(true, Ordering::SeqCst);
    let logs_for_task = ctx.logs.clone();
    let observers_for_task = ctx.observers.clone();
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
            observers_for_task,
            api_base_url,
            worker_id,
            auth_token,
            job,
            started_at,
            task_kind,
            full_prompt,
            prompt_preview,
        )
        .await;
        busy_flag.store(false, Ordering::SeqCst);
    });
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(coverage_nightly, coverage(off))]
async fn run_offered_job(
    sender: WsSender,
    engine: Arc<dyn Engine>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    observers: WorkerObservers,
    api_base_url: String,
    worker_id: String,
    auth_token: String,
    job: crate::types::JobClaim,
    started_at: chrono::DateTime<chrono::Utc>,
    task_kind: crate::types::TaskKind,
    full_prompt: String,
    prompt_preview: String,
) {
    let start = std::time::Instant::now();
    // Pass the studio's `ModelSource` to the engine so sd-cpp /
    // llama-cpp know which files to load.  Required on every offer
    // — the studio refuses to promote a job without a model source
    // and the worker refuses any claim that lacks one.
    let dispatch = tokio::task::spawn_blocking({
        let model = job.model.clone();
        let model_source = job.model_source.clone();
        let task_for_engine = job.task.clone();
        let engine = engine.clone();
        move || -> Result<TaskResult> {
            engine.dispatch_with_source(&model, task_for_engine, &model_source)
        }
    })
    .await;

    let job_id = job.job_id.clone();
    // Tracks the outcome we record into the RecentJob ring once every
    // dispatch arm below has either succeeded or surfaced an error.
    // The default value here only survives if the match falls through
    // without assigning, which is unreachable; we keep it as a
    // belt-and-braces default so the recent-jobs ring is never left
    // half-populated by a future code-path that forgets to assign.
    #[allow(unused_assignments)]
    let mut outcome = JobOutcome::Failed {
        reason: "dispatch did not run to completion".to_string(),
    };
    match dispatch {
        Ok(Ok(result)) => {
            push_log_with_observers(
                &logs,
                Some(&observers),
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
                        let prompt = full_prompt.clone();
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
                        push_log_with_observers(
                            &logs,
                            Some(&observers),
                            "error",
                            "ws",
                            &msg,
                            Some(job_id.clone()),
                        );
                        outcome = JobOutcome::Failed {
                            reason: msg.clone(),
                        };
                        let fail_result = sender
                            .send(&WorkerInbound::Fail {
                                job_id: job_id.clone(),
                                error: msg,
                                retryable: true,
                            })
                            .await;
                        record_fail_send(&fail_result, &job_id, &logs, &observers);
                    } else {
                        push_log_with_observers(
                            &logs,
                            Some(&observers),
                            "info",
                            "ws",
                            "binary upload ok",
                            Some(job_id.clone()),
                        );
                        outcome = JobOutcome::Completed;
                        // The studio's HTTP `/complete` handler defers a
                        // `notifyJobCompleted` RPC to the
                        // WorkerConnections DO; that's the canonical
                        // "offer next job" nudge.  Sending an extra
                        // `ReadyForMore` here races that flow: both can
                        // call `offerNextFor` concurrently, double-
                        // reserve the session's `currentJob` slot, and
                        // ship two `Offer` frames — the second `Accept`
                        // then trips the studio's `session not
                        // authenticated`-shaped `accept for unknown
                        // jobId` invariant and the DO kills the
                        // session.  See:
                        //   apps/studio/src/worker/modules/graphics/
                        //     WorkerConnections/orchestrator.ts (commitOffer)
                    }
                }
                TaskResult::Llm { json } | TaskResult::AudioStt { json } => {
                    // Mirror the binary path: branch on the send result
                    // so a dropped `completeJson` frame is recorded as a
                    // failure (never a false-positive `Completed`) and a
                    // successful send leaves an explicit completion
                    // breadcrumb in the logs + shipped studio logs,
                    // symmetric with the binary path's "binary upload ok".
                    match sender
                        .send(&WorkerInbound::CompleteJson {
                            job_id: job_id.clone(),
                            result: json,
                            prompt: Some(full_prompt.clone()),
                        })
                        .await
                    {
                        Ok(()) => {
                            push_log_with_observers(
                                &logs,
                                Some(&observers),
                                "info",
                                "ws",
                                "json result sent",
                                Some(job_id.clone()),
                            );
                            outcome = JobOutcome::Completed;
                        }
                        Err(e) => {
                            let msg = format!("failed to send result: {e}");
                            push_log_with_observers(
                                &logs,
                                Some(&observers),
                                "error",
                                "ws",
                                &msg,
                                Some(job_id.clone()),
                            );
                            outcome = JobOutcome::Failed { reason: msg };
                        }
                    }
                }
            }
        }
        Ok(Err(e)) => {
            warn!(target: TRACE_TARGET, error = %e, "engine dispatch failed");
            push_log_with_observers(
                &logs,
                Some(&observers),
                "error",
                "ws",
                &format!("dispatch failed: {e}"),
                Some(job_id.clone()),
            );
            outcome = JobOutcome::Failed {
                reason: e.to_string(),
            };
            let fail_result = sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: !is_unsupported_kind(&e),
                })
                .await;
            record_fail_send(&fail_result, &job_id, &logs, &observers);
        }
        Err(e) => {
            push_log_with_observers(
                &logs,
                Some(&observers),
                "error",
                "ws",
                &format!("dispatch task panic: {e}"),
                Some(job_id.clone()),
            );
            outcome = JobOutcome::Failed {
                reason: e.to_string(),
            };
            let fail_result = sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: true,
                })
                .await;
            record_fail_send(&fail_result, &job_id, &logs, &observers);
        }
    }

    // Surface the finished job to the UI: clear the current-job slot
    // and push a RecentJob entry into the ring.
    *observers.current_job.lock() = None;
    record_recent_job(
        &observers,
        RecentJob {
            job_id: job_id.clone(),
            kind: task_kind,
            model: job.model.clone(),
            prompt: prompt_preview,
            outcome,
            started_at,
            finished_at: chrono::Utc::now(),
        },
    );
}

#[cfg_attr(coverage_nightly, coverage(off))]
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

#[cfg_attr(coverage_nightly, coverage(off))]
fn spawn_heartbeat_pump(
    capabilities: WorkerCapabilities,
    sender: WsSender,
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
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
            // The capability snapshot is captured once at session
            // start.  Only `auto_enabled` (the pause flag) and the
            // current job id can change between heartbeats.
            let mut caps = capabilities.clone();
            caps.auto_enabled = !paused.load(Ordering::SeqCst);
            let current_job_id = if busy.load(Ordering::SeqCst) {
                Some("in-flight".to_string())
            } else {
                None
            };
            if let Err(e) = sender
                .send(&WorkerInbound::Heartbeat {
                    capabilities: caps,
                    current_job_id,
                })
                .await
            {
                warn!(target: TRACE_TARGET, error = %e, "heartbeat send failed");
                break;
            }
        }
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
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

#[cfg_attr(coverage_nightly, coverage(off))]
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

fn backoff_for(attempt: u32, schedule: SessionSchedule) -> Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let raw_ms = schedule.base_backoff_ms.saturating_mul(factor);
    Duration::from_millis(raw_ms.min(schedule.max_backoff_ms))
}

/// Decide whether a just-attempted offer-response send (accept /
/// reject) warrants a session-level breadcrumb.
///
/// Returns `None` on success: the happy path is already implied by the
/// surrounding "dispatched" / "rejecting offer: paused" breadcrumbs, so
/// re-logging it would only add per-job noise.  Returns
/// `Some(("error", …))` when the send failed — a dropped accept leaves
/// the worker running a job the studio never marked accepted, and a
/// dropped reject leaves the offer reserved on a paused worker until it
/// times out.  The transport layer already logs the failure locally on
/// `studio_worker::ws::client`, but only a session-level breadcrumb
/// reaches the UI's Logs tab and the studio-shipped log view with the
/// offending `job_id` attached.  Pure so the wording + level are
/// unit-tested without a live WS sink.
fn offer_response_breadcrumb(
    label: &str,
    job_id: &str,
    result: &WsResult<()>,
) -> Option<(&'static str, String)> {
    match result {
        Ok(()) => None,
        Err(e) => Some((
            "error",
            format!("{label} send failed for offer {job_id}: {e}"),
        )),
    }
}

/// Decide whether a just-attempted `Fail`-frame send warrants a
/// session-level breadcrumb.
///
/// Returns `None` on success: the caller already logged the underlying
/// job failure (the upload error, dispatch error, or panic), so a `Fail`
/// frame that lands needs no second per-job line.  Returns
/// `Some(("error", …))` when the send itself failed — a dropped `Fail`
/// leaves the studio believing the job is still in flight (reserved on
/// the session's `currentJob` slot) until it times out, with no local
/// record that the notification never landed.  The transport layer logs
/// the drop locally on `studio_worker::ws::client`, but only a
/// session-level breadcrumb reaches the UI's Logs tab and the
/// studio-shipped log view with the offending `job_id` attached.  Pure
/// so the wording + level are unit-tested without a live WS sink.
fn fail_send_breadcrumb(job_id: &str, result: &WsResult<()>) -> Option<(&'static str, String)> {
    match result {
        Ok(()) => None,
        Err(e) => Some((
            "error",
            format!("failed to notify studio of job {job_id} failure: {e}"),
        )),
    }
}

/// Push a session-level breadcrumb when a `Fail`-frame send dropped.
///
/// Trivial glue over [`fail_send_breadcrumb`]: the three job-failure
/// arms (upload error, dispatch error, dispatch panic) all notify the
/// studio with a `Fail` frame and then call this, so a dropped
/// notification is recorded with the `job_id` attached instead of being
/// swallowed by `let _ = sender.send(...)`.
fn record_fail_send(
    result: &WsResult<()>,
    job_id: &str,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    observers: &WorkerObservers,
) {
    if let Some((level, message)) = fail_send_breadcrumb(job_id, result) {
        push_log_with_observers(
            logs,
            Some(observers),
            level,
            "ws",
            &message,
            Some(job_id.to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_response_breadcrumb_is_silent_on_success() {
        // The happy path is already implied by the surrounding
        // "dispatched" / "rejecting offer: paused" breadcrumbs, so a
        // successful accept / reject send must not add per-job noise.
        assert!(offer_response_breadcrumb("accept", "j-1", &Ok(())).is_none());
        assert!(offer_response_breadcrumb("reject", "j-2", &Ok(())).is_none());
    }

    #[test]
    fn offer_response_breadcrumb_reports_accept_send_failure() {
        let (level, msg) =
            offer_response_breadcrumb("accept", "j-1", &Err(WsClientError::ConnectionClosed))
                .expect("a failed accept send must surface a breadcrumb");
        assert_eq!(level, "error");
        assert!(msg.contains("accept send failed"), "got: {msg}");
        assert!(msg.contains("j-1"), "must name the job: {msg}");
        assert!(
            msg.contains("connection closed"),
            "must carry the cause: {msg}"
        );
    }

    #[test]
    fn offer_response_breadcrumb_reports_reject_send_failure() {
        let (level, msg) = offer_response_breadcrumb(
            "reject",
            "j-9",
            &Err(WsClientError::Transport("sink gone".into())),
        )
        .expect("a failed reject send must surface a breadcrumb");
        assert_eq!(level, "error");
        assert!(msg.contains("reject send failed"), "got: {msg}");
        assert!(msg.contains("j-9"), "must name the job: {msg}");
        assert!(msg.contains("sink gone"), "must carry the cause: {msg}");
    }

    #[test]
    fn fail_send_breadcrumb_is_silent_on_success() {
        // The underlying job failure (upload / dispatch / panic) is
        // already logged by the caller, so a Fail-frame that lands must
        // not add a second per-job line.
        assert!(fail_send_breadcrumb("j-1", &Ok(())).is_none());
    }

    #[test]
    fn fail_send_breadcrumb_reports_send_failure() {
        let (level, msg) = fail_send_breadcrumb("j-7", &Err(WsClientError::ConnectionClosed))
            .expect("a dropped Fail send must surface a breadcrumb");
        assert_eq!(level, "error");
        assert!(msg.contains("j-7"), "must name the job: {msg}");
        assert!(
            msg.contains("connection closed"),
            "must carry the cause: {msg}"
        );
    }

    #[test]
    fn fail_send_breadcrumb_carries_transport_cause() {
        let (level, msg) =
            fail_send_breadcrumb("j-3", &Err(WsClientError::Transport("sink gone".into())))
                .expect("a dropped Fail send must surface a breadcrumb");
        assert_eq!(level, "error");
        assert!(msg.contains("j-3"), "must name the job: {msg}");
        assert!(msg.contains("sink gone"), "must carry the cause: {msg}");
    }

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
    fn has_credentials_false_when_either_missing() {
        let mut cfg = crate::config::Config::default();
        let shared = crate::config::shared(cfg.clone());
        assert!(!has_credentials(&shared), "both missing");
        cfg.worker_id = Some("w-1".into());
        let shared = crate::config::shared(cfg.clone());
        assert!(!has_credentials(&shared), "only worker_id");
        cfg.worker_id = None;
        cfg.auth_token = Some("tok".into());
        let shared = crate::config::shared(cfg.clone());
        assert!(!has_credentials(&shared), "only auth_token");
    }

    #[test]
    fn has_credentials_true_when_both_present() {
        let cfg = crate::config::Config {
            worker_id: Some("w-1".into()),
            auth_token: Some("tok".into()),
            ..crate::config::Config::default()
        };
        let shared = crate::config::shared(cfg);
        assert!(has_credentials(&shared));
    }

    #[test]
    fn has_credentials_false_when_empty_strings() {
        let cfg = crate::config::Config {
            worker_id: Some("".into()),
            auth_token: Some("".into()),
            ..crate::config::Config::default()
        };
        let shared = crate::config::shared(cfg);
        assert!(!has_credentials(&shared));
    }
}
