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
use std::convert::TryFrom;
use std::time::Duration;

use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::{Error as TError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::ws::types::{WorkerInbound, WorkerOutbound};

pub const SUBPROTOCOL: &str = "studio-worker-v1";
/// Mirrors the same prefix the HTTP `ApiClient` mounts under.  Stays
/// single-sourced with the API's Hono `basePath('/api')` + outer
/// `/graphics` mount.
const API_PREFIX: &str = "/graphics/api";

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
            TError::ConnectionClosed | TError::AlreadyClosed => WsClientError::ConnectionClosed,
            other => WsClientError::Transport(other.to_string()),
        }
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
pub async fn connect(base_url: &str, worker_id: &str, auth_token: &str) -> WsResult<WsClient> {
    let url = build_connect_url(base_url, worker_id)?;
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

    let (stream, _response) = tokio_tungstenite::connect_async(request).await?;
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
        let text =
            serde_json::to_string(frame).map_err(|e| WsClientError::Protocol(e.to_string()))?;
        let mut guard = self.sink.lock().await;
        guard
            .send(Message::Text(text.into()))
            .await
            .map_err(WsClientError::from)
    }

    pub async fn close(&self, code: u16, reason: &str) -> WsResult<()> {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.to_owned().into(),
        };
        let mut guard = self.sink.lock().await;
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            guard.send(Message::Close(Some(frame))),
        )
        .await;
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
            match item {
                Ok(Message::Text(text)) => {
                    let frame: WorkerOutbound = serde_json::from_str(&text)
                        .map_err(|e| WsClientError::Protocol(e.to_string()))?;
                    return Ok(Some(frame));
                }
                Ok(Message::Binary(_)) => {
                    return Err(WsClientError::Protocol(
                        "unexpected binary frame".to_string(),
                    ));
                }
                Ok(Message::Close(frame)) => {
                    self.closed = true;
                    return Err(close_frame_to_error(frame));
                }
                Ok(_) => continue,
                Err(e) => return Err(WsClientError::from(e)),
            }
        }
        self.closed = true;
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
        let text =
            serde_json::to_string(frame).map_err(|e| WsClientError::Protocol(e.to_string()))?;
        self.sink
            .send(Message::Text(text.into()))
            .await
            .map_err(WsClientError::from)
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
            match item {
                Ok(Message::Text(text)) => {
                    let frame: WorkerOutbound = serde_json::from_str(&text)
                        .map_err(|e| WsClientError::Protocol(e.to_string()))?;
                    return Ok(Some(frame));
                }
                Ok(Message::Binary(_)) => {
                    return Err(WsClientError::Protocol(
                        "unexpected binary frame".to_string(),
                    ));
                }
                Ok(Message::Close(frame)) => {
                    self.closed = true;
                    return Err(close_frame_to_error(frame));
                }
                Ok(_) => continue, // ping/pong/empty — keep reading
                Err(e) => return Err(WsClientError::from(e)),
            }
        }
        self.closed = true;
        Ok(None)
    }

    /// Best-effort graceful close.  Idempotent.
    pub async fn close(&mut self, code: u16, reason: &str) -> WsResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.to_owned().into(),
        };
        // Wrap in a short timeout so a stuck peer can't hang shutdown.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.sink.send(Message::Close(Some(frame))),
        )
        .await;
        Ok(())
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
}
