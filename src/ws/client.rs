//! `tokio-tungstenite`-backed client for the studio WS worker channel.
//!
//! Responsibilities:
//!  - coerce `http(s)://` API URLs to `ws(s)://` and append `/connect`
//!  - attach `Authorization: Bearer <token>` and the
//!    `studio-worker-v1` sub-protocol header to the upgrade
//!  - map 401 upgrade responses + 4001 close codes to a typed
//!    `WsClientError::AuthFailed` so the runtime can surface a
//!    friendly hint
//!  - serialise `WorkerInbound` to JSON text frames and parse
//!    `WorkerOutbound` from incoming frames
//!  - clean shutdown via `WsClient::close()`
//!  - emit structured `tracing` breadcrumbs (target
//!    `studio_worker::ws::client`) at the transport boundary so
//!    connect / recv / send failures are never silent.  The session
//!    discards recv errors in its generic `Disconnected(_)` arm and
//!    fires `let _ = sender.send(...)` for accept / reject / fail /
//!    completeJson, so this layer is the only place those faults can
//!    surface.  Mirrors the `studio_worker::http` breadcrumb contract.
use std::convert::TryFrom;
use std::time::{Duration, Instant};

use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::Response;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::{Error as TError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};
use url::Url;

use crate::ws::types::{WorkerInbound, WorkerOutbound};

pub const SUBPROTOCOL: &str = "studio-worker-v1";

/// Tracing target used for every event emitted by the WS client.
/// Stable so operators can filter with
/// `RUST_LOG=studio_worker::ws::client=debug` without enabling
/// wire-level tungstenite logging.
const TRACE_TARGET: &str = "studio_worker::ws::client";
/// Mirrors the same prefix the HTTP `ApiClient` mounts under.  Stays
/// single-sourced with the API's Hono `basePath('/api')` + outer
/// `/graphics` mount.
const API_PREFIX: &str = "/graphics/api";

/// Upper bound on a single connect attempt (TCP + TLS + WS upgrade). Without it a peer that accepts
/// the socket but stalls the upgrade hangs the reconnect loop forever (no logs, no progress) — the
/// connect-side twin of the read-idle-timeout on an established session.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Result wrapper for WS-client operations.
pub type WsResult<T> = Result<T, WsClientError>;

/// Errors surfaced by the client.  All variants carry just enough
/// context to log a useful warning + to drive the reconnect policy.
#[derive(Debug, thiserror::Error)]
pub enum WsClientError {
    /// Upgrade returned 401 or the server closed with 4001.
    #[error("auth failed: {reason}")]
    AuthFailed { reason: String },

    /// Server closed for a reason other than auth failure.  The runtime
    /// treats this as a transient drop and tries to reconnect.
    #[error("connection closed by server")]
    ConnectionClosed,

    /// Anything else (DNS, TLS, timeout).
    #[error("ws transport error: {0}")]
    Transport(String),

    /// Frame couldn't be parsed as JSON `WorkerOutbound`.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<TError> for WsClientError {
    fn from(value: TError) -> Self {
        match value {
            TError::Http(response) if response.status() == StatusCode::UNAUTHORIZED => {
                WsClientError::AuthFailed {
                    reason: "401 on websocket upgrade".to_string(),
                }
            }
            // Any other upgrade status (500/502/503/429 …) is a
            // transient server-side fault the reconnect loop retries.
            // Carry the status + body so the studio's error
            // `reference` id reaches the operator's log.
            TError::Http(response) => {
                WsClientError::Transport(http_upgrade_error_message(&response))
            }
            TError::ConnectionClosed | TError::AlreadyClosed => WsClientError::ConnectionClosed,
            other => WsClientError::Transport(other.to_string()),
        }
    }
}

/// Upper bound on the number of characters of a non-401 HTTP upgrade
/// body we fold into the transport-error breadcrumb.  Enough to carry
/// the studio's JSON error `reference` id without letting a stray HTML
/// error page flood the log line.
const HTTP_ERROR_BODY_MAX_CHARS: usize = 300;

/// Render a non-401 HTTP upgrade failure into a transport-error string
/// that surfaces both the status and (when present) the response body.
/// tungstenite's own `Error::Http` Display keeps only the status, but
/// the studio answers a failed `/connect` upgrade with a JSON body
/// carrying an error `reference` id — the same value Sentry shows for
/// the matching studio-side event.  Folding it into the breadcrumb
/// lets an operator correlate the worker's reconnect-loop warning with
/// the studio's logged failure.  The body is decoded lossily, trimmed,
/// and clipped to [`HTTP_ERROR_BODY_MAX_CHARS`].
fn http_upgrade_error_message(response: &Response<Option<Vec<u8>>>) -> String {
    let status = response.status();
    let body = response.body().as_deref().and_then(|bytes| {
        let decoded = String::from_utf8_lossy(bytes);
        let trimmed = decoded.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(clip_error_body(trimmed))
    });
    match body {
        Some(b) => format!("HTTP {status} on websocket upgrade: {b}"),
        None => format!("HTTP {status} on websocket upgrade"),
    }
}

/// Clip a decoded error body to [`HTTP_ERROR_BODY_MAX_CHARS`],
/// appending an ellipsis when truncated.  Char-based so a multibyte
/// body can't be split mid-codepoint.
fn clip_error_body(body: &str) -> String {
    if body.chars().count() > HTTP_ERROR_BODY_MAX_CHARS {
        let mut clipped: String = body.chars().take(HTTP_ERROR_BODY_MAX_CHARS).collect();
        clipped.push('\u{2026}');
        clipped
    } else {
        body.to_string()
    }
}

/// Coerce an `http://...api` base URL to the WS URL the server expects.
fn build_connect_url(base_url: &str, worker_id: &str) -> WsResult<Url> {
    let mut url = Url::parse(base_url)
        .map_err(|e| WsClientError::Transport(format!("invalid base url: {e}")))?;
    let new_scheme = match url.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        "ws" | "wss" => None, // already in WS form
        other => {
            return Err(WsClientError::Transport(format!(
                "unsupported scheme: {other}"
            )))
        }
    };
    if let Some(scheme) = new_scheme {
        url.set_scheme(scheme)
            .map_err(|_| WsClientError::Transport("set_scheme failed".to_string()))?;
    }
    let trimmed_path = url.path().trim_end_matches('/');
    // Append the studio's `/graphics/api` prefix unless the caller has
    // already baked it into `base_url` (matches what `ApiClient::url`
    // does on the HTTP side).
    let prefixed = if trimmed_path.ends_with(API_PREFIX) {
        trimmed_path.to_string()
    } else {
        format!("{trimmed_path}{API_PREFIX}")
    };
    let new_path = format!("{prefixed}/workers/{worker_id}/connect");
    url.set_path(&new_path);
    Ok(url)
}

/// Establish the WebSocket session.  Sends the upgrade with the bearer
/// token + sub-protocol header and returns a ready-to-use client.
///
/// Emits a `debug` breadcrumb on success and a `warn` on failure so a
/// dead studio, bad DNS, or TLS fault is visible without the caller
/// having to log it.
pub async fn connect(base_url: &str, worker_id: &str, auth_token: &str) -> WsResult<WsClient> {
    let started = Instant::now();
    let result = connect_inner(base_url, worker_id, auth_token, CONNECT_TIMEOUT).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => debug!(
            target: TRACE_TARGET,
            op = "connect",
            worker_id,
            elapsed_ms,
            "websocket established"
        ),
        Err(e) => warn!(
            target: TRACE_TARGET,
            op = "connect",
            worker_id,
            elapsed_ms,
            error = %e,
            "websocket connect failed"
        ),
    }
    result
}

async fn connect_inner(
    base_url: &str,
    worker_id: &str,
    auth_token: &str,
    connect_timeout: Duration,
) -> WsResult<WsClient> {
    let url = build_connect_url(base_url, worker_id)?;
    debug!(
        target: TRACE_TARGET,
        op = "connect",
        worker_id,
        url = %url,
        "opening websocket"
    );
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(WsClientError::from)?;
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        HeaderValue::try_from(format!("Bearer {auth_token}"))
            .map_err(|e| WsClientError::Transport(format!("invalid auth header: {e}")))?,
    );
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );

    let (stream, _response) = match tokio::time::timeout(
        connect_timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(result) => result?,
        Err(_elapsed) => {
            return Err(WsClientError::Transport(format!(
                "connect timed out after {connect_timeout:?}"
            )))
        }
    };
    let (sink, source) = stream.split();
    Ok(WsClient {
        sink,
        source,
        closed: false,
    })
}

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsSource = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// Active worker-side WS session.  Cheap to construct, expensive to
/// drop (closes the socket gracefully).
#[allow(missing_debug_implementations)]
pub struct WsClient {
    sink: WsSink,
    source: WsSource,
    closed: bool,
}

impl WsClient {
    /// Split the client into a cheap-to-clone `WsSender` and a
    /// single-owner `WsReceiver`.  Used by the runtime so heartbeat,
    /// log-shipper, and engine-dispatch tasks can all push frames
    /// concurrently while a dedicated task drains the receive side.
    pub fn split(self) -> (WsSender, WsReceiver) {
        let sink = Arc::new(Mutex::new(self.sink));
        (
            WsSender { sink },
            WsReceiver {
                source: self.source,
                closed: false,
            },
        )
    }
}

/// Cheap-to-clone send half.  All senders share one `Mutex` over the
/// underlying sink so writes from heartbeat / log-shipper / engine
/// dispatch tasks are serialised correctly.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct WsSender {
    sink: Arc<Mutex<WsSink>>,
}

impl WsSender {
    pub async fn send(&self, frame: &WorkerInbound) -> WsResult<()> {
        let text = serde_json::to_string(frame).map_err(|e| {
            let err = WsClientError::Protocol(e.to_string());
            log_send_error(frame, &err);
            err
        })?;
        let mut guard = self.sink.lock().await;
        guard.send(Message::Text(text.into())).await.map_err(|e| {
            let err = WsClientError::from(e);
            log_send_error(frame, &err);
            err
        })
    }

    pub async fn close(&self, code: u16, reason: &str) -> WsResult<()> {
        debug!(target: TRACE_TARGET, op = "close", code, reason, "closing websocket");
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.to_owned().into(),
        };
        let mut guard = self.sink.lock().await;
        if tokio::time::timeout(
            Duration::from_secs(5),
            guard.send(Message::Close(Some(frame))),
        )
        .await
        .is_err()
        {
            warn!(target: TRACE_TARGET, op = "close", code, "timed out sending close frame");
        }
        Ok(())
    }
}

/// Single-owner receive half.  Owned by the session's reader task.
#[allow(missing_debug_implementations)]
pub struct WsReceiver {
    source: WsSource,
    closed: bool,
}

impl WsReceiver {
    /// Read the next outbound frame.  Same semantics as
    /// `WsClient::recv` — silent close → `Ok(None)`, close frame with
    /// 4001 → `AuthFailed`, other closes → `ConnectionClosed`.
    pub async fn recv(&mut self) -> WsResult<Option<WorkerOutbound>> {
        if self.closed {
            return Ok(None);
        }
        while let Some(item) = self.source.next().await {
            match classify_incoming(item) {
                RecvStep::Yield(frame) => return Ok(Some(frame)),
                RecvStep::Skip => continue,
                RecvStep::Fail(e) => return Err(e),
                RecvStep::Closed(e) => {
                    self.closed = true;
                    return Err(e);
                }
            }
        }
        self.closed = true;
        debug!(target: TRACE_TARGET, op = "recv", "stream ended (no close frame)");
        Ok(None)
    }
}

impl std::fmt::Debug for WsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsClient")
            .field("closed", &self.closed)
            .finish()
    }
}

impl WsClient {
    /// Send a typed inbound frame as a JSON text frame.
    pub async fn send(&mut self, frame: &WorkerInbound) -> WsResult<()> {
        let text = serde_json::to_string(frame).map_err(|e| {
            let err = WsClientError::Protocol(e.to_string());
            log_send_error(frame, &err);
            err
        })?;
        self.sink
            .send(Message::Text(text.into()))
            .await
            .map_err(|e| {
                let err = WsClientError::from(e);
                log_send_error(frame, &err);
                err
            })
    }

    /// Receive the next typed outbound frame.  Returns `Ok(None)` on
    /// a clean close (no error frame), `Err` on auth or transport
    /// failures, or `Ok(Some(frame))` for normal traffic.  Pings and
    /// other control frames are swallowed silently.
    pub async fn recv(&mut self) -> WsResult<Option<WorkerOutbound>> {
        if self.closed {
            return Ok(None);
        }
        while let Some(item) = self.source.next().await {
            match classify_incoming(item) {
                RecvStep::Yield(frame) => return Ok(Some(frame)),
                RecvStep::Skip => continue,
                RecvStep::Fail(e) => return Err(e),
                RecvStep::Closed(e) => {
                    self.closed = true;
                    return Err(e);
                }
            }
        }
        self.closed = true;
        debug!(target: TRACE_TARGET, op = "recv", "stream ended (no close frame)");
        Ok(None)
    }

    /// Best-effort graceful close.  Idempotent.
    pub async fn close(&mut self, code: u16, reason: &str) -> WsResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        debug!(target: TRACE_TARGET, op = "close", code, reason, "closing websocket");
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.to_owned().into(),
        };
        // Wrap in a short timeout so a stuck peer can't hang shutdown.
        if tokio::time::timeout(
            Duration::from_secs(5),
            self.sink.send(Message::Close(Some(frame))),
        )
        .await
        .is_err()
        {
            warn!(target: TRACE_TARGET, op = "close", code, "timed out sending close frame");
        }
        Ok(())
    }
}

/// Human-readable label for an inbound frame, used in send-failure
/// breadcrumbs so operators can tell a dropped `accept` from a dropped
/// `heartbeat`.
fn frame_label(frame: &WorkerInbound) -> &'static str {
    match frame {
        WorkerInbound::Hello(_) => "hello",
        WorkerInbound::Heartbeat { .. } => "heartbeat",
        WorkerInbound::Accept { .. } => "accept",
        WorkerInbound::Reject { .. } => "reject",
        WorkerInbound::CompleteJson { .. } => "completeJson",
        WorkerInbound::Fail { .. } => "fail",
        WorkerInbound::LogBatch { .. } => "logBatch",
        WorkerInbound::ReadyForMore => "readyForMore",
    }
}

/// Log a failed frame send.  Callers (the session) routinely fire
/// `let _ = sender.send(...)`, so without this a dropped `accept` /
/// `fail` / `completeJson` would vanish without trace.
fn log_send_error(frame: &WorkerInbound, err: &WsClientError) {
    warn!(
        target: TRACE_TARGET,
        op = "send",
        frame = frame_label(frame),
        error = %err,
        "failed to send frame"
    );
}

/// Interpretation of a single raw WS message during `recv`.  Splitting
/// this out keeps the two `recv` loops (split + non-split) identical
/// and routes every error / close through one logging site.
enum RecvStep {
    /// Decoded application frame to hand back to the caller.
    Yield(WorkerOutbound),
    /// Control / empty frame (ping / pong) — keep reading.
    Skip,
    /// Error to surface without latching the receiver closed.
    Fail(WsClientError),
    /// Server sent a close frame — latch closed, then surface the error.
    Closed(WsClientError),
}

/// Classify one incoming message, emitting a tracing breadcrumb for
/// every failure / close so transport faults are never silent.
fn classify_incoming(item: Result<Message, TError>) -> RecvStep {
    match item {
        Ok(Message::Text(text)) => match serde_json::from_str::<WorkerOutbound>(&text) {
            Ok(frame) => RecvStep::Yield(frame),
            Err(e) => {
                warn!(
                    target: TRACE_TARGET,
                    op = "recv",
                    error = %e,
                    "dropping unparseable text frame"
                );
                RecvStep::Fail(WsClientError::Protocol(e.to_string()))
            }
        },
        Ok(Message::Binary(_)) => {
            warn!(
                target: TRACE_TARGET,
                op = "recv",
                "rejecting unexpected binary frame"
            );
            RecvStep::Fail(WsClientError::Protocol(
                "unexpected binary frame".to_string(),
            ))
        }
        Ok(Message::Close(frame)) => {
            let err = close_frame_to_error(frame);
            match &err {
                WsClientError::AuthFailed { reason } => warn!(
                    target: TRACE_TARGET,
                    op = "recv",
                    reason = %reason,
                    "server closed connection: auth failed"
                ),
                _ => debug!(
                    target: TRACE_TARGET,
                    op = "recv",
                    "server closed connection"
                ),
            }
            RecvStep::Closed(err)
        }
        // ping / pong / empty — keep reading.
        Ok(_) => RecvStep::Skip,
        Err(e) => {
            let mapped = WsClientError::from(e);
            match &mapped {
                // A clean close surfaces here as ConnectionClosed on
                // some transports; keep it at debug to avoid noise on
                // expected reconnect churn.
                WsClientError::ConnectionClosed => debug!(
                    target: TRACE_TARGET,
                    op = "recv",
                    "connection closed by peer"
                ),
                other => warn!(
                    target: TRACE_TARGET,
                    op = "recv",
                    error = %other,
                    "transport error while reading frame"
                ),
            }
            RecvStep::Fail(mapped)
        }
    }
}

fn close_frame_to_error(frame: Option<CloseFrame>) -> WsClientError {
    if let Some(frame) = frame {
        let code: u16 = frame.code.into();
        if code == 4001 {
            return WsClientError::AuthFailed {
                reason: format!("server closed 4001: {}", frame.reason),
            };
        }
    }
    WsClientError::ConnectionClosed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_connect_url_http_to_ws() {
        let url = build_connect_url("http://api.example/graphics/api", "w-1").unwrap();
        assert_eq!(url.scheme(), "ws");
        assert!(url.path().ends_with("/workers/w-1/connect"));
    }

    #[test]
    fn build_connect_url_https_to_wss() {
        let url = build_connect_url("https://api.example/graphics/api/", "w-2").unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.path(), "/graphics/api/workers/w-2/connect");
    }

    #[test]
    fn build_connect_url_appends_graphics_api_prefix_when_missing() {
        let url = build_connect_url("http://localhost:9790", "w-3").unwrap();
        assert_eq!(url.scheme(), "ws");
        assert_eq!(url.path(), "/graphics/api/workers/w-3/connect");
    }

    #[test]
    fn build_connect_url_preserves_existing_ws_scheme() {
        let url = build_connect_url("ws://localhost:9790/x", "w").unwrap();
        assert_eq!(url.scheme(), "ws");
    }

    #[test]
    fn build_connect_url_rejects_unknown_scheme() {
        let err = build_connect_url("ftp://nope/", "w").unwrap_err();
        assert!(matches!(err, WsClientError::Transport(_)));
    }

    #[test]
    fn build_connect_url_rejects_invalid_url() {
        let err = build_connect_url("not a url", "w").unwrap_err();
        assert!(matches!(err, WsClientError::Transport(_)));
    }

    #[test]
    fn close_frame_4001_maps_to_auth_failed() {
        let frame = CloseFrame {
            code: CloseCode::Library(4001),
            reason: "bad token".into(),
        };
        let err = close_frame_to_error(Some(frame));
        assert!(matches!(err, WsClientError::AuthFailed { .. }));
    }

    #[test]
    fn close_frame_other_codes_map_to_connection_closed() {
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        };
        let err = close_frame_to_error(Some(frame));
        assert!(matches!(err, WsClientError::ConnectionClosed));
    }

    #[test]
    fn close_frame_missing_maps_to_connection_closed() {
        let err = close_frame_to_error(None);
        assert!(matches!(err, WsClientError::ConnectionClosed));
    }

    #[test]
    fn transport_error_round_trips_through_from_impl() {
        let inner = TError::AlreadyClosed;
        let mapped: WsClientError = inner.into();
        assert!(matches!(mapped, WsClientError::ConnectionClosed));
    }

    // -----------------------------------------------------------------
    // Structured tracing breadcrumbs.  The transport layer must never
    // swallow a failure silently: callers (the session) discard recv
    // errors in their generic `Disconnected(_)` arm and use
    // `let _ = sender.send(...)` for accept/reject/fail/completeJson,
    // so the only place those faults can surface is here.  Mirrors the
    // `studio_worker::http` breadcrumb contract.
    // -----------------------------------------------------------------
    use crate::test_support::capture;

    #[test]
    fn classify_rejects_binary_frame_with_warn() {
        let logs = capture(|| {
            let step = classify_incoming(Ok(Message::Binary(vec![1, 2, 3].into())));
            assert!(matches!(step, RecvStep::Fail(WsClientError::Protocol(_))));
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(
            logs.contains("studio_worker::ws::client"),
            "expected target, got: {logs}"
        );
        assert!(logs.contains("op=\"recv\""), "expected op field: {logs}");
        assert!(logs.contains("binary"), "expected reason: {logs}");
    }

    #[test]
    fn classify_warns_on_unparseable_text_frame() {
        let logs = capture(|| {
            let step = classify_incoming(Ok(Message::Text("not json".into())));
            assert!(matches!(step, RecvStep::Fail(WsClientError::Protocol(_))));
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(logs.contains("op=\"recv\""), "expected op field: {logs}");
    }

    #[test]
    fn classify_warns_on_4001_close_frame() {
        let logs = capture(|| {
            let frame = CloseFrame {
                code: CloseCode::Library(4001),
                reason: "invalid auth token".into(),
            };
            let step = classify_incoming(Ok(Message::Close(Some(frame))));
            assert!(matches!(
                step,
                RecvStep::Closed(WsClientError::AuthFailed { .. })
            ));
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(logs.contains("auth failed"), "expected reason: {logs}");
    }

    #[test]
    fn classify_debug_logs_on_normal_close_frame() {
        let logs = capture(|| {
            let frame = CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            };
            let step = classify_incoming(Ok(Message::Close(Some(frame))));
            assert!(matches!(
                step,
                RecvStep::Closed(WsClientError::ConnectionClosed)
            ));
        });
        assert!(logs.contains("DEBUG"), "expected DEBUG, got: {logs}");
        assert!(!logs.contains("WARN"), "normal close must not warn: {logs}");
        assert!(logs.contains("server closed"), "expected message: {logs}");
    }

    #[test]
    fn classify_yields_valid_frame_without_warning() {
        let logs = capture(|| {
            let json = serde_json::json!({ "type": "heartbeatAck" }).to_string();
            let step = classify_incoming(Ok(Message::Text(json.into())));
            assert!(matches!(
                step,
                RecvStep::Yield(WorkerOutbound::HeartbeatAck)
            ));
        });
        assert!(
            !logs.contains("WARN"),
            "a valid frame should not warn: {logs}"
        );
    }

    #[test]
    fn classify_skips_control_frames() {
        assert!(matches!(
            classify_incoming(Ok(Message::Ping(Vec::new().into()))),
            RecvStep::Skip
        ));
        assert!(matches!(
            classify_incoming(Ok(Message::Pong(Vec::new().into()))),
            RecvStep::Skip
        ));
    }

    #[test]
    fn frame_label_names_every_inbound_variant() {
        use crate::types::WorkerCapabilities;
        let caps = WorkerCapabilities {
            machine_name: String::new(),
            username: String::new(),
            agent_version: String::new(),
            engine: String::new(),
            vram_total_gb: 0.0,
            vram_threshold_gb: 0.0,
            auto_enabled: false,
            auto_start: false,
            supported_models: vec![],
            task_kinds: vec![],
            supported_models_per_kind: Default::default(),
        };
        assert_eq!(
            frame_label(&WorkerInbound::Hello(crate::ws::types::HelloFrame {
                auth_token: String::new(),
                capabilities: caps.clone(),
            })),
            "hello"
        );
        assert_eq!(
            frame_label(&WorkerInbound::Heartbeat {
                capabilities: caps,
                current_job_id: None,
            }),
            "heartbeat"
        );
        assert_eq!(
            frame_label(&WorkerInbound::Accept { job_id: "j".into() }),
            "accept"
        );
        assert_eq!(
            frame_label(&WorkerInbound::Reject {
                job_id: "j".into(),
                reason: "r".into(),
                code: None,
            }),
            "reject"
        );
        assert_eq!(
            frame_label(&WorkerInbound::CompleteJson {
                job_id: "j".into(),
                result: serde_json::Value::Null,
                prompt: None,
            }),
            "completeJson"
        );
        assert_eq!(
            frame_label(&WorkerInbound::Fail {
                job_id: "j".into(),
                error: "e".into(),
                retryable: true,
            }),
            "fail"
        );
        assert_eq!(
            frame_label(&WorkerInbound::LogBatch { entries: vec![] }),
            "logBatch"
        );
        assert_eq!(frame_label(&WorkerInbound::ReadyForMore), "readyForMore");
    }

    #[test]
    fn send_error_logs_warn_with_frame_label() {
        let logs = capture(|| {
            log_send_error(
                &WorkerInbound::Accept {
                    job_id: "j-1".into(),
                },
                &WsClientError::ConnectionClosed,
            );
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(logs.contains("op=\"send\""), "expected op field: {logs}");
        assert!(
            logs.contains("frame=\"accept\""),
            "expected frame label: {logs}"
        );
    }

    #[tokio::test]
    async fn connect_times_out_against_a_stalling_upgrade() {
        // A listener that accepts the TCP connection but never answers the WS upgrade. Without the
        // connect timeout this blocks forever; with it, a transport error must surface fast.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _accepted = listener.accept().await; // hold the socket, never upgrade
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let url = format!("http://{addr}/graphics/api");
        let started = Instant::now();
        let result = connect_inner(&url, "w", "tok", Duration::from_millis(150)).await;
        assert!(
            matches!(result, Err(WsClientError::Transport(_))),
            "expected a transport timeout, got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "connect must time out promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn connect_failure_logs_warn_breadcrumb() {
        // Port 1 has nothing listening, so the upgrade fails fast with
        // a transport error.  No server required — deterministic.
        let logs = capture(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(connect("http://127.0.0.1:1/graphics/api", "w-err", "tok"));
            assert!(result.is_err(), "connect to a dead port should fail");
        });
        assert!(logs.contains("WARN"), "expected WARN, got: {logs}");
        assert!(logs.contains("op=\"connect\""), "expected op field: {logs}");
        assert!(
            logs.contains("websocket connect failed"),
            "expected message: {logs}"
        );
        assert!(
            logs.contains("worker_id=\"w-err\""),
            "expected worker_id field: {logs}"
        );
    }

    // -----------------------------------------------------------------
    // From<TError> — HTTP upgrade-error mapping.  A 401 stays a typed
    // AuthFailed so the runtime surfaces a friendly token hint; every
    // other status is a transient server-side fault the reconnect loop
    // retries.  For the latter we fold the studio's response body —
    // which carries the JSON error `reference` id Sentry also shows —
    // into the breadcrumb so the worker's reconnect warning can be
    // correlated with the studio-side failure.  tungstenite's own
    // `Error::Http` Display keeps only the status and drops the body.
    // -----------------------------------------------------------------

    fn http_error(status: u16, body: Option<&[u8]>) -> TError {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(status)
            .body(body.map(<[u8]>::to_vec))
            .expect("a valid response");
        TError::Http(response)
    }

    #[test]
    fn http_401_upgrade_maps_to_auth_failed_ignoring_body() {
        let err = WsClientError::from(http_error(401, Some(b"any body")));
        assert!(
            matches!(err, WsClientError::AuthFailed { .. }),
            "401 must stay AuthFailed, got {err:?}"
        );
    }

    #[test]
    fn http_500_upgrade_surfaces_status_and_reference_body() {
        let err = WsClientError::from(http_error(
            500,
            Some(b"internal error; reference = q1mtuhheh7en3lfqoofvgfgd"),
        ));
        let WsClientError::Transport(msg) = err else {
            panic!("a non-401 HTTP error must map to Transport, got {err:?}");
        };
        assert!(msg.contains("500"), "status must be present: {msg}");
        assert!(
            msg.contains("q1mtuhheh7en3lfqoofvgfgd"),
            "the studio's error reference id must survive into the breadcrumb: {msg}"
        );
    }

    #[test]
    fn http_503_upgrade_without_body_keeps_just_the_status() {
        let err = WsClientError::from(http_error(503, None));
        let WsClientError::Transport(msg) = err else {
            panic!("expected Transport, got {err:?}");
        };
        assert!(msg.contains("503"), "status must be present: {msg}");
        assert!(
            !msg.trim_end().ends_with(':'),
            "a bodyless error must not leave a dangling colon: {msg}"
        );
    }

    #[test]
    fn http_upgrade_blank_body_is_treated_as_no_body() {
        let err = WsClientError::from(http_error(500, Some(b"   \n\t ")));
        let WsClientError::Transport(msg) = err else {
            panic!("expected Transport, got {err:?}");
        };
        assert!(
            !msg.trim_end().ends_with(':'),
            "a whitespace-only body must not leave a dangling colon: {msg}"
        );
    }

    #[test]
    fn http_upgrade_error_body_is_clipped() {
        let big = "x".repeat(5_000);
        let err = WsClientError::from(http_error(502, Some(big.as_bytes())));
        let WsClientError::Transport(msg) = err else {
            panic!("expected Transport, got {err:?}");
        };
        assert!(
            msg.chars().count() < big.len(),
            "a huge error page must be clipped, got {} chars",
            msg.chars().count()
        );
        assert!(
            msg.contains('\u{2026}'),
            "a clipped body must carry an ellipsis: {msg}"
        );
    }
}
