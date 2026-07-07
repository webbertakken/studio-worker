//! Long-running WebSocket session that owns the worker's lifecycle.
//!
//! Replaces the four polling loops (`spawn_heartbeat`, `spawn_claim_loop`,
//! `spawn_log_shipper`, plus the implicit completion path) with a single
//! `spawn_ws_session` coordinator + a small handful of helper tasks that
//! all push frames through a shared `WsSender`.
//!
//! Reconnect policy: on a transport error or non-auth close, back off
//! `BASE_BACKOFF_MS * 2^attempt` and try again, up to
//! `cfg.ws_reconnect_attempts`.  Out of retries → return `Err` and the
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
    is_unsupported_kind, prompt_for, push_log_with_observers, record_recent_job, set_session_state,
    truncate_prompt, wait_with_stop, CurrentJob, JobOutcome, RecentJob, SessionState,
    WorkerObservers,
};
use crate::types::{LogEntry, TaskResult};
use crate::ws::client::{connect, WsClientError, WsResult, WsSender};
use crate::ws::types::{HelloFrame, JobOfferClaim, WorkerInbound, WorkerOutbound};

/// Tracing target used for every event emitted by the session.
const TRACE_TARGET: &str = "studio_worker::ws::session";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const SHUTDOWN_TICK: Duration = Duration::from_millis(250);
const BASE_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 30_000;
/// Reconnect attempts before giving up, when the operator hasn't
/// pinned `ws_reconnect_attempts`.  `0` = retry forever.
///
/// A zero-touch worker must never permanently give up while
/// unattended: the turnkey install runs the UI (or an autostart tray)
/// with **no** service manager to restart a process that exits, so a
/// laptop that sleeps through a wifi outage used to wake as a dead
/// worker after 5 failed reconnects.  Infinite-with-capped-backoff is
/// the only default that keeps "approve once, never touch again" true.
/// Operators who want fail-fast under systemd can still set a finite
/// `ws_reconnect_attempts`.
const DEFAULT_RECONNECT_ATTEMPTS: u32 = 0;
/// Extra attempts for the multipart result upload when the studio
/// returns a 5xx / transport error.  A blip is far cheaper to retry
/// than the full GPU regeneration a reported `Fail` causes.
const UPLOAD_RETRIES: u32 = 2;
/// Base pause between upload retries (grows linearly per attempt).
const UPLOAD_RETRY_PAUSE: Duration = Duration::from_secs(1);
/// If no frame (not even a `heartbeatAck`) arrives from the studio within this window, treat the
/// connection as dead and tear the session down. The studio acks every heartbeat (~5s), so a live
/// connection always yields a frame well inside this budget; the only time it elapses is a
/// half-open / dead-peer socket where the reader would otherwise block on `source.next()` forever.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Tunables for the session loop — dialed down in tests.
#[derive(Debug, Clone, Copy)]
pub struct SessionSchedule {
    pub heartbeat: Duration,
    pub log_flush: Duration,
    pub shutdown_tick: Duration,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    /// Reader gives up + reports a disconnect if no server frame arrives within this window.
    pub read_idle_timeout: Duration,
}

impl Default for SessionSchedule {
    fn default() -> Self {
        Self {
            heartbeat: HEARTBEAT_INTERVAL,
            log_flush: LOG_FLUSH_INTERVAL,
            shutdown_tick: SHUTDOWN_TICK,
            base_backoff_ms: BASE_BACKOFF_MS,
            max_backoff_ms: MAX_BACKOFF_MS,
            read_idle_timeout: READ_IDLE_TIMEOUT,
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
            // Generous vs the 5ms heartbeat so the existing fast tests never trip it; the
            // silent-connection test overrides this with a tiny value to exercise the timeout.
            read_idle_timeout: Duration::from_secs(5),
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
                set_session_state(&observers, SessionState::WaitingForApproval);
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

        set_session_state(&observers, SessionState::Connecting);
        let welcomed = AtomicBool::new(false);
        match run_one_session(
            &cfg, &stop, &logs, &busy, &paused, &observers, schedule, &welcomed,
        )
        .await
        {
            Ok(SessionOutcome::Stopped) => {
                set_session_state(&observers, SessionState::Stopped);
                return Ok(());
            }
            Ok(SessionOutcome::AuthFailed(reason)) => {
                set_session_state(
                    &observers,
                    SessionState::AuthFailed {
                        reason: reason.clone(),
                    },
                );
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
                set_session_state(
                    &observers,
                    SessionState::Fatal {
                        reason: reason.clone(),
                    },
                );
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
            outcome @ (Ok(SessionOutcome::Disconnected) | Err(_)) => {
                // A session that successfully connected shouldn't count its later drop toward the
                // connect-failure cap — only consecutive failures to connect should accumulate, so
                // a long-lived worker isn't killed by transient mid-session disconnects.
                if welcomed.load(Ordering::SeqCst) {
                    attempt = 0;
                }
                attempt += 1;
                if reconnect_exhausted(max_attempts, attempt) {
                    set_session_state(
                        &observers,
                        SessionState::Fatal {
                            reason: format!("gave up after {attempt} reconnect attempts"),
                        },
                    );
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
                set_session_state(&observers, SessionState::Reconnecting { attempt });
                let backoff = backoff_for(attempt, schedule);
                push_log_with_observers(
                    &logs,
                    Some(&observers),
                    "warn",
                    "ws",
                    &reconnect_breadcrumb(outcome.as_ref().err(), attempt, backoff),
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
            SessionEvent::Frame(WorkerOutbound::Welcome {
                worker_id: wid,
                server_time,
            }) => {
                push_log_with_observers(
                    logs,
                    Some(observers),
                    "info",
                    "ws",
                    &welcome_breadcrumb(&wid, &server_time),
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
// Eight collaborators (config + shared flags + observers + schedule + welcomed signal);
// grouping them adds indirection without improving readability.
#[allow(clippy::too_many_arguments)]
async fn run_one_session(
    cfg: &SharedConfig,
    stop: &Arc<AtomicBool>,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    busy: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    observers: &WorkerObservers,
    schedule: SessionSchedule,
    welcomed: &AtomicBool,
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
    // A threshold above the card's detected VRAM makes the studio offer
    // jobs this GPU can't fit — they OOM on load.  Flag the
    // misconfiguration on the handshake so the OOM has an operator-facing
    // cause instead of surfacing only as a failed job.
    if let Some(warning) = crate::runtime::vram_threshold_warning(&capabilities) {
        push_log_with_observers(logs, Some(observers), "warn", "ws", &warning, None);
    }
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
    let reader = spawn_reader(receiver, event_tx.clone(), schedule.read_idle_timeout);

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
        WelcomeOutcome::Welcomed => {
            welcomed.store(true, Ordering::SeqCst);
            set_session_state(observers, SessionState::Connected);
        }
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

    // Heartbeat task.  Reuses the engine handle built for the Hello
    // frame (rebuilding fires every engine's registration log every
    // 5s and floods the logs) but rebuilds the capability snapshot
    // from the live config each tick, so operator edits (e.g. a new
    // VRAM threshold saved from the UI's Config tab) reach the studio
    // without waiting for a reconnect.
    let engine_arc: Arc<dyn Engine> = engine.into();
    let heartbeat = spawn_heartbeat_pump(
        cfg.clone(),
        engine_arc.clone(),
        sender.clone(),
        stop.clone(),
        paused.clone(),
        logs.clone(),
        observers.clone(),
        schedule,
    );

    // Log shipper task.
    let log_shipper = spawn_log_shipper_pump(sender.clone(), logs.clone(), stop.clone(), schedule);

    // Shutdown observer: ticks until stop flag is set, then drops the channel.
    let shutdown_observer = spawn_shutdown_observer(stop.clone(), event_tx.clone(), schedule);
    drop(event_tx);

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

    // The session is ending (disconnect or shutdown). The heartbeat / log-shipper /
    // shutdown-observer pumps only break on the *global* stop flag or a send failure, so on a
    // silent-but-open socket — where heartbeat sends still succeed into the TCP buffer — they would
    // loop forever and block this function from returning, which is exactly the post-job reconnect
    // hang. Abort them so teardown is bounded regardless of socket state, then best-effort close +
    // drain the aborted handles (await returns promptly with Cancelled).
    reader.abort();
    heartbeat.abort();
    log_shipper.abort();
    shutdown_observer.abort();
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
/// around — keeps clippy's `too_many_arguments` lint happy.  Cloning
/// is cheap: every field is an `Arc`, a cloneable sender, or a small
/// `String`.
#[derive(Clone)]
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
                WorkerOutbound::Welcome {
                    worker_id: wid,
                    server_time,
                } => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        &welcome_breadcrumb(&wid, &server_time),
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
                WorkerOutbound::CompleteAck { job_id } => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        &result_ack_breadcrumb("completion", &job_id),
                        Some(job_id),
                    );
                }
                WorkerOutbound::FailAck { job_id } => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        &result_ack_breadcrumb("failure", &job_id),
                        Some(job_id),
                    );
                }
                WorkerOutbound::HeartbeatAck => {
                    // Heartbeat acks fire every ~5s; logging each would
                    // flood the operator log with no diagnostic value
                    // (a genuinely missed ack already surfaces via the
                    // read-idle timeout + reconnect breadcrumb).
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
        &offer_received_breadcrumb(
            &job_id,
            &claim.game_id,
            &claim.asset_name,
            &claim.model,
            claim.vram_gb_estimate,
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
        spawn_reject_offer(
            ctx.sender.clone(),
            ctx.logs.clone(),
            ctx.observers.clone(),
            job_id,
            "worker paused by operator",
            crate::ws::types::RejectCode::Paused,
        );
        return;
    }
    let reservation = match crate::job_gate::JobGate::from_shared(ctx.busy.clone()).try_reserve() {
        Some(reservation) => reservation,
        None => {
            push_log_with_observers(
                &ctx.logs,
                Some(&ctx.observers),
                "info",
                "ws",
                &format!("rejecting offer {job_id}: worker is already busy"),
                Some(job_id.clone()),
            );
            spawn_reject_offer(
                ctx.sender.clone(),
                ctx.logs.clone(),
                ctx.observers.clone(),
                job_id,
                "worker already has an in-flight job",
                crate::ws::types::RejectCode::Busy,
            );
            return;
        }
    };
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

    let ctx = ctx.clone();
    tokio::spawn(async move {
        // Held for the whole job; its Drop frees the worker slot on
        // every exit path (accept failure, success, or a panic).
        let _reservation = reservation;
        let accept_result = ctx
            .sender
            .send(&WorkerInbound::Accept {
                job_id: job_id.clone(),
            })
            .await;
        if let Some((level, message)) = offer_response_breadcrumb("accept", &job_id, &accept_result)
        {
            push_log_with_observers(
                &ctx.logs,
                Some(&ctx.observers),
                level,
                "ws",
                &message,
                Some(job_id.clone()),
            );
        }
        if accept_result.is_err() {
            return;
        }

        // Surface the job to the UI's Jobs tab — bounded preview only.
        *ctx.observers.current_job.lock() = Some(CurrentJob {
            job_id: job_id.clone(),
            kind: task_kind,
            model: job.model.clone(),
            prompt: prompt_preview.clone(),
            started_at,
        });

        run_offered_job(
            &ctx,
            job,
            started_at,
            task_kind,
            full_prompt,
            prompt_preview,
        )
        .await;
    });
}

fn spawn_reject_offer(
    sender: WsSender,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    observers: WorkerObservers,
    job_id: String,
    reason: &'static str,
    code: crate::ws::types::RejectCode,
) {
    tokio::spawn(async move {
        let result = sender
            .send(&WorkerInbound::Reject {
                job_id: job_id.clone(),
                reason: reason.to_string(),
                code: Some(code),
            })
            .await;
        if let Some((level, message)) = offer_response_breadcrumb("reject", &job_id, &result) {
            push_log_with_observers(&logs, Some(&observers), level, "ws", &message, Some(job_id));
        }
    });
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn run_offered_job(
    ctx: &SessionContext,
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
        let engine = ctx.engine.clone();
        move || -> Result<TaskResult> {
            engine.dispatch_with_source(&model, task_for_engine, &model_source)
        }
    })
    .await;

    let job_id = job.job_id.clone();
    // Every arm produces the outcome as a value, so the compiler
    // proves the RecentJob ring always records a real outcome — no
    // mutable default that survives a forgotten assignment.
    let outcome = match dispatch {
        Ok(Ok(result)) => {
            push_log_with_observers(
                &ctx.logs,
                Some(&ctx.observers),
                "info",
                "ws",
                &format!("{} dispatched in {:?}", task_kind.as_str(), start.elapsed()),
                Some(job_id.clone()),
            );
            deliver_result(ctx, &job_id, result, &full_prompt).await
        }
        Ok(Err(e)) => {
            warn!(target: TRACE_TARGET, error = %e, "engine dispatch failed");
            push_log_with_observers(
                &ctx.logs,
                Some(&ctx.observers),
                "error",
                "ws",
                &format!("dispatch failed: {e}"),
                Some(job_id.clone()),
            );
            let fail_result = ctx
                .sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: !is_unsupported_kind(&e),
                })
                .await;
            record_fail_send(&fail_result, &job_id, &ctx.logs, &ctx.observers);
            JobOutcome::Failed {
                reason: e.to_string(),
            }
        }
        Err(e) => {
            push_log_with_observers(
                &ctx.logs,
                Some(&ctx.observers),
                "error",
                "ws",
                &format!("dispatch task panic: {e}"),
                Some(job_id.clone()),
            );
            let fail_result = ctx
                .sender
                .send(&WorkerInbound::Fail {
                    job_id: job_id.clone(),
                    error: e.to_string(),
                    retryable: true,
                })
                .await;
            record_fail_send(&fail_result, &job_id, &ctx.logs, &ctx.observers);
            JobOutcome::Failed {
                reason: e.to_string(),
            }
        }
    };

    // Surface the finished job to the UI: clear the current-job slot
    // and push a RecentJob entry into the ring.
    *ctx.observers.current_job.lock() = None;
    record_recent_job(
        &ctx.observers,
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

/// Deliver a successful engine result to the studio and return the
/// outcome to record.  Binary outputs travel the multipart HTTP
/// `/complete` route (R2 doesn't fit in WS frames); JSON outputs
/// travel the WS `completeJson` frame.
#[cfg_attr(coverage_nightly, coverage(off))]
async fn deliver_result(
    ctx: &SessionContext,
    job_id: &str,
    result: TaskResult,
    full_prompt: &str,
) -> JobOutcome {
    match result {
        TaskResult::Image { bytes, ext }
        | TaskResult::AudioTts { bytes, ext }
        | TaskResult::Video { bytes, ext } => {
            let upload_result = tokio::task::spawn_blocking({
                let api_base_url = ctx.api_base_url.clone();
                let job_id = job_id.to_string();
                let auth_token = ctx.auth_token.clone();
                let worker_id = ctx.worker_id.clone();
                let prompt = full_prompt.to_string();
                move || -> Result<()> {
                    let api = ApiClient::new(api_base_url)?;
                    api.complete_with_retry(
                        &worker_id,
                        &auth_token,
                        &job_id,
                        &ext,
                        &prompt,
                        bytes,
                        UPLOAD_RETRIES,
                        UPLOAD_RETRY_PAUSE,
                    )
                }
            })
            .await;
            let msg = match upload_result {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(e) => Some(format!("upload task panic: {e}")),
            };
            match msg {
                Some(msg) => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "error",
                        "ws",
                        &msg,
                        Some(job_id.to_string()),
                    );
                    let fail_result = ctx
                        .sender
                        .send(&WorkerInbound::Fail {
                            job_id: job_id.to_string(),
                            error: msg.clone(),
                            retryable: true,
                        })
                        .await;
                    record_fail_send(&fail_result, job_id, &ctx.logs, &ctx.observers);
                    JobOutcome::Failed { reason: msg }
                }
                None => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        "binary upload ok",
                        Some(job_id.to_string()),
                    );
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
                    JobOutcome::Completed
                }
            }
        }
        TaskResult::Llm { json } | TaskResult::AudioStt { json } => {
            // Mirror the binary path: branch on the send result so a
            // dropped `completeJson` frame is recorded as a failure
            // (never a false-positive `Completed`) and a successful
            // send leaves an explicit completion breadcrumb, symmetric
            // with the binary path's "binary upload ok".
            match ctx
                .sender
                .send(&WorkerInbound::CompleteJson {
                    job_id: job_id.to_string(),
                    result: json,
                    prompt: Some(full_prompt.to_string()),
                })
                .await
            {
                Ok(()) => {
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "info",
                        "ws",
                        "json result sent",
                        Some(job_id.to_string()),
                    );
                    JobOutcome::Completed
                }
                Err(e) => {
                    let msg = format!("failed to send result: {e}");
                    push_log_with_observers(
                        &ctx.logs,
                        Some(&ctx.observers),
                        "error",
                        "ws",
                        &msg,
                        Some(job_id.to_string()),
                    );
                    JobOutcome::Failed { reason: msg }
                }
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn spawn_reader(
    mut receiver: crate::ws::client::WsReceiver,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    read_idle_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Bound the wait so a half-open / dead-peer socket can't block the reader forever.
            // A live studio acks every heartbeat (~5s), so a frame always lands well inside the
            // window; elapsing it means the connection is gone and the session must reconnect.
            match tokio::time::timeout(read_idle_timeout, receiver.recv()).await {
                Ok(Ok(Some(frame))) => {
                    if event_tx.send(SessionEvent::Frame(frame)).is_err() {
                        break;
                    }
                }
                Ok(Ok(None)) => {
                    let _ =
                        event_tx.send(SessionEvent::Disconnected(WsClientError::ConnectionClosed));
                    break;
                }
                Ok(Err(e)) => {
                    let _ = event_tx.send(SessionEvent::Disconnected(e));
                    break;
                }
                Err(_elapsed) => {
                    let _ = event_tx.send(SessionEvent::Disconnected(WsClientError::Transport(
                        format!(
                            "no frames from server for {:?}; treating connection as dead",
                            read_idle_timeout
                        ),
                    )));
                    break;
                }
            }
        }
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
// Eight collaborators (config + engine + sender + shared flags + logs + observers + schedule);
// grouping them adds indirection without improving readability.
#[allow(clippy::too_many_arguments)]
fn spawn_heartbeat_pump(
    cfg: SharedConfig,
    engine: Arc<dyn Engine>,
    sender: WsSender,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    observers: WorkerObservers,
    schedule: SessionSchedule,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(schedule.heartbeat);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Seed with the pause flag's value at session start so the first
        // tick never logs a spurious transition; only genuine operator
        // toggles during the session ship a breadcrumb.
        let mut last_paused = paused.load(Ordering::SeqCst);
        loop {
            interval.tick().await;
            if stop.load(Ordering::SeqCst) {
                break;
            }
            // A Pause / Resume from any source (Status tab, tray menu)
            // only emits a local `tracing` breadcrumb; ship the actual
            // transition so the studio's shipped-log view and the UI's
            // Logs tab record why the worker started / stopped claiming.
            let now_paused = paused.load(Ordering::SeqCst);
            if let Some(message) = pause_transition_breadcrumb(last_paused, now_paused) {
                push_log_with_observers(&logs, Some(&observers), "info", "ws", message, None);
            }
            last_paused = now_paused;
            // Rebuild the snapshot from the live config so operator
            // edits (VRAM threshold, auto-start) propagate on the
            // next tick instead of on the next reconnect.
            let caps = crate::runtime::build_capabilities_with(&cfg.lock(), &*engine, !now_paused);
            let current_job_id = heartbeat_current_job_id(&observers);
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

fn heartbeat_current_job_id(observers: &WorkerObservers) -> Option<String> {
    observers
        .current_job
        .lock()
        .as_ref()
        .map(|job| job.job_id.clone())
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
            let frame = WorkerInbound::LogBatch { entries: batch };
            if let Err(e) = sender.send(&frame).await {
                warn!(target: TRACE_TARGET, error = %e, "log batch send failed; requeueing batch");
                // Put the batch back so it ships on the next session
                // instead of vanishing with this one.
                if let WorkerInbound::LogBatch { entries } = frame {
                    crate::runtime::restore_unshipped(&logs, entries);
                }
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

/// Whether the reconnect loop should stop trying.  `max_attempts == 0`
/// means retry forever (the default) — see [`DEFAULT_RECONNECT_ATTEMPTS`].
/// A finite cap gives up once `attempt` exceeds it.
fn reconnect_exhausted(max_attempts: u32, attempt: u32) -> bool {
    max_attempts > 0 && attempt > max_attempts
}

fn backoff_for(attempt: u32, schedule: SessionSchedule) -> Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let raw_ms = schedule.base_backoff_ms.saturating_mul(factor);
    Duration::from_millis(raw_ms.min(schedule.max_backoff_ms))
}

/// Build the operator breadcrumb for a session that dropped or never
/// established, surfacing the underlying error so a reconnect loop is
/// never opaque about *why* it's retrying.
///
/// A plain mid-session disconnect (`Ok(SessionOutcome::Disconnected)`)
/// carries no error and keeps the legacy "disconnected; reconnect
/// attempt …" wording that the studio-shipped log view and the
/// `ws_session_full_loop` contract test key on.  An `Err` outcome — an
/// engine that failed to build, or a `hello` frame that never made it
/// onto the wire — previously vanished into that same wording with no
/// cause, leaving an operator staring at an endless reconnect loop with
/// nothing to diagnose.  The full anyhow chain (`{:#}`) rides along so a
/// wrapped root cause isn't truncated to its outer context.  Pure so
/// the wording is unit-tested without a live WS round-trip.
fn reconnect_breadcrumb(error: Option<&anyhow::Error>, attempt: u32, backoff: Duration) -> String {
    let in_ms = backoff.as_millis();
    match error {
        Some(e) => format!("session error: {e:#}; reconnect attempt {attempt} in {in_ms}ms"),
        None => format!("disconnected; reconnect attempt {attempt} in {in_ms}ms"),
    }
}

/// Operator-facing breadcrumb for the studio's `Welcome` frame.
///
/// The studio stamps `server_time` (its clock at the moment it
/// authenticated this worker) onto every `Welcome`, but it used to be
/// deserialised and dropped — the line named only the worker id. With
/// it surfaced, an operator can spot clock skew between the worker host
/// and the studio straight from the UI's Logs tab and the
/// studio-shipped log view: skew distorts heartbeat-timeout reasoning,
/// auth-token expiry windows, and log-timestamp correlation across the
/// two sides. Pure so the wording is unit-tested without a live
/// welcome.
fn welcome_breadcrumb(worker_id: &str, server_time: &str) -> String {
    format!("server welcomed {worker_id} server_time={server_time}")
}

/// Operator-facing breadcrumb summarising an incoming job offer.
///
/// The studio populates `game_id` + `asset_name` on every offer, but
/// they used to be deserialised and dropped — the line only named the
/// model + vram estimate, so a worker fielding offers across many games
/// gave no clue which game / asset each job served. Surfacing both
/// (data already on the wire) lets operators triage "which game's jobs
/// are failing on this box" straight from the UI's Logs tab and the
/// studio-shipped log view. Pure so the wording is unit-tested without
/// a live offer.
fn offer_received_breadcrumb(
    job_id: &str,
    game_id: &str,
    asset_name: &str,
    model: &str,
    vram_gb_estimate: f32,
) -> String {
    format!(
        "offer received {job_id} game={game_id} asset={asset_name} model={model} vram={vram_gb_estimate}"
    )
}

/// Operator-facing breadcrumb for the studio's `CompleteAck` /
/// `FailAck` frames.
///
/// The studio sends one of these the moment it has persisted a job's
/// result (the binary landed in R2, or the `completeJson` / `Fail`
/// frame updated the row). Both used to be silently dropped on the
/// "acks are best-effort; ignore" arm, so the worker's own
/// "binary upload ok" / completeJson breadcrumb was the last word on a
/// job: an operator triaging a job that ran twice (worker reported
/// done, studio never persisted, the job timed out + requeued) had no
/// signal telling them whether the studio ever acknowledged the
/// result. Surfacing the ack closes the job lifecycle in the UI's Logs
/// tab and the studio-shipped log view. `HeartbeatAck` stays unlogged:
/// it fires every ~5s and a genuinely missed ack already surfaces via
/// the read-idle timeout + reconnect breadcrumb. Pure so the wording is
/// unit-tested without a live ack.
fn result_ack_breadcrumb(outcome: &str, job_id: &str) -> String {
    format!("studio confirmed {outcome} of job {job_id}")
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

/// Operator-facing breadcrumb for a change in the runtime pause flag,
/// or `None` when the flag is unchanged since the previous heartbeat
/// tick.
///
/// A Pause / Resume from the Status tab or tray menu only emits a local
/// `tracing` breadcrumb (stdout / Sentry) naming the source; it never
/// enters the worker's shipped log stream.  So the studio's shipped-log
/// view and the UI's Logs tab used to show `auto_enabled=false`
/// heartbeats with no record of *why* the worker stopped claiming.  The
/// heartbeat pump calls this each tick and ships the transition through
/// `push_log_with_observers`, so a toggle from *any* source reaches the
/// operator-facing surfaces.  Pure so the wording is unit-tested without
/// driving the pump.
fn pause_transition_breadcrumb(prev: bool, now: bool) -> Option<&'static str> {
    match (prev, now) {
        (false, true) => Some("claiming paused by operator; new offers are rejected until resumed"),
        (true, false) => Some("claiming resumed by operator; accepting new offers again"),
        _ => None,
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

    // (worker-reservation exclusivity now lives in `job_gate::tests`.)

    #[test]
    fn heartbeat_current_job_id_uses_actual_job_id() {
        let observers = WorkerObservers::default();
        assert_eq!(heartbeat_current_job_id(&observers), None);
        *observers.current_job.lock() = Some(CurrentJob {
            job_id: "job-42".into(),
            kind: crate::types::TaskKind::Image,
            model: "synthetic".into(),
            prompt: "prompt".into(),
            started_at: chrono::Utc::now(),
        });
        assert_eq!(
            heartbeat_current_job_id(&observers).as_deref(),
            Some("job-42")
        );
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
            read_idle_timeout: Duration::from_secs(1),
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
    fn reconnect_is_infinite_by_default_and_finite_only_when_pinned() {
        // The default (max_attempts == 0) must never give up — an
        // unattended worker with no service manager has to keep
        // reconnecting through a long outage instead of dying.
        assert!(!reconnect_exhausted(0, 1));
        assert!(!reconnect_exhausted(0, 10_000));
        // Regression guard: the old default of 5 killed the worker on
        // the 6th failure; the current default must not.
        assert!(!reconnect_exhausted(DEFAULT_RECONNECT_ATTEMPTS, 6));
        assert_eq!(DEFAULT_RECONNECT_ATTEMPTS, 0, "default must be infinite");
        // A pinned finite cap still gives up past the cap.
        assert!(!reconnect_exhausted(5, 5));
        assert!(reconnect_exhausted(5, 6));
    }

    #[test]
    fn reconnect_breadcrumb_keeps_legacy_wording_for_a_plain_disconnect() {
        // A mid-session drop carries no error; the exact wording the
        // studio-shipped log view and the `ws_session_full_loop`
        // contract test key on must be preserved.
        let msg = reconnect_breadcrumb(None, 3, Duration::from_millis(800));
        assert_eq!(msg, "disconnected; reconnect attempt 3 in 800ms");
    }

    #[test]
    fn reconnect_breadcrumb_surfaces_the_underlying_error() {
        // An `Err` outcome (engine build failure, `hello` send failure)
        // used to vanish into the plain "disconnected" line, leaving an
        // operator with an opaque endless reconnect loop. The cause must
        // now ride along while still naming the attempt + backoff.
        let err = anyhow!("hello send failed: connection closed");
        let msg = reconnect_breadcrumb(Some(&err), 2, Duration::from_millis(400));
        assert!(
            msg.contains("reconnect attempt 2 in 400ms"),
            "must still name attempt + backoff: {msg}"
        );
        assert!(
            msg.contains("hello send failed: connection closed"),
            "must carry the cause: {msg}"
        );
    }

    #[test]
    fn reconnect_breadcrumb_includes_the_full_error_chain() {
        // anyhow context chains must reach the operator so a wrapped
        // root cause isn't truncated to just the outer context.
        let err = anyhow!("driver missing").context("engine build failed");
        let msg = reconnect_breadcrumb(Some(&err), 1, Duration::from_millis(100));
        assert!(msg.contains("engine build failed"), "got: {msg}");
        assert!(
            msg.contains("driver missing"),
            "must include the root cause: {msg}"
        );
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

    #[test]
    fn pause_transition_breadcrumb_is_silent_when_unchanged() {
        // No flag change since the previous tick — the pump must not add
        // a per-tick log line on every 5s heartbeat.
        assert!(pause_transition_breadcrumb(false, false).is_none());
        assert!(pause_transition_breadcrumb(true, true).is_none());
    }

    #[test]
    fn pause_transition_breadcrumb_reports_pause_and_resume() {
        // A genuine operator toggle must ship an info-level breadcrumb
        // naming the new claiming state so the studio's shipped-log view
        // and the UI's Logs tab record why the worker stopped / resumed.
        let paused = pause_transition_breadcrumb(false, true).expect("a pause must be reported");
        assert!(
            paused.contains("paused by operator"),
            "expected a pause message, got: {paused}"
        );
        let resumed = pause_transition_breadcrumb(true, false).expect("a resume must be reported");
        assert!(
            resumed.contains("resumed by operator"),
            "expected a resume message, got: {resumed}"
        );
    }

    #[test]
    fn welcome_breadcrumb_surfaces_server_time() {
        // The studio stamps `server_time` (its clock at the moment it
        // authenticated this worker) onto every `Welcome`; it used to be
        // deserialised and dropped, so an operator couldn't spot clock
        // skew between the worker host and the studio. The breadcrumb
        // must keep the legacy "server welcomed <id>" wording and add the
        // server time alongside it.
        let line = welcome_breadcrumb("worker-7", "2026-06-15T21:00:00Z");
        assert!(
            line.contains("server welcomed worker-7"),
            "expected the legacy wording + worker id, got: {line}"
        );
        assert!(
            line.contains("server_time=2026-06-15T21:00:00Z"),
            "expected the server time, got: {line}"
        );
    }

    #[test]
    fn offer_received_breadcrumb_names_game_and_asset() {
        // The studio sends `game_id` + `asset_name` on every offer; both
        // used to be deserialised and dropped, so an operator fielding
        // offers across many games couldn't tell which game / asset each
        // job served. The breadcrumb must surface both alongside the
        // model + vram estimate it already reported.
        let line = offer_received_breadcrumb(
            "j-1",
            "game-of-elements",
            "game-of-elements/creatures/aurora-fox",
            "sd-cpp:flux",
            12.5,
        );
        assert!(
            line.contains("offer received j-1"),
            "expected the job id, got: {line}"
        );
        assert!(
            line.contains("game=game-of-elements"),
            "expected the game id, got: {line}"
        );
        assert!(
            line.contains("asset=game-of-elements/creatures/aurora-fox"),
            "expected the asset name, got: {line}"
        );
        assert!(
            line.contains("model=sd-cpp:flux"),
            "expected the model, got: {line}"
        );
        assert!(line.contains("vram=12.5"), "expected the vram, got: {line}");
    }

    #[test]
    fn result_ack_breadcrumb_names_the_outcome_and_job() {
        // The studio sends `CompleteAck` / `FailAck` the moment it has
        // persisted a job's result; both used to be silently dropped on
        // the "acks are best-effort; ignore" arm, so the worker's own
        // "binary upload ok" / completeJson line was the last word on a
        // job. An operator triaging a job that ran twice (worker
        // reported done, studio never persisted, job requeued) had no
        // signal telling them whether the studio acknowledged the
        // result. The breadcrumb must name both the outcome and the
        // offending job id.
        assert_eq!(
            result_ack_breadcrumb("completion", "j-1"),
            "studio confirmed completion of job j-1"
        );
        assert_eq!(
            result_ack_breadcrumb("failure", "j-2"),
            "studio confirmed failure of job j-2"
        );
    }
}
