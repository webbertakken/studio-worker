//! The LAN streaming listener: `ws://<host>:<port>/transcribe?token=...`.
//! A stream token (minted on the loopback API) names the model; the
//! session runs on that loaded model's lane until it finalises, the
//! client leaves, or the model is unloaded.
//!
//! Binds the LAN on purpose (a phone streams to it); the stream token is the
//! guard.  One session per model at a time (`try_with_lane`).

use super::session::{ClientFrame, Next, ServerFrame, StreamSession};
pub use super::tokens::StreamTokens;
use crate::host::{Lane, LoadedModel, ModelHost};
use crate::runtime::{record_local_job, truncate_prompt, JobOutcome, RecentJob, WorkerObservers};
use crate::types::TaskKind;
use chrono::Utc;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

/// Port the listener binds when none is configured (next to the local
/// API's 4787).  Safe range: any free port.
pub const DEFAULT_STREAM_PORT: u16 = 4798;
/// The one path served.
pub const STREAM_PATH: &str = "/transcribe";
const TRACE_TARGET: &str = "studio_worker::stt_stream";
/// Read timeout while streaming: how often an idle session checks for an
/// unload.  Safe range 20..=500 ms.
const POLL: Duration = Duration::from_millis(100);
/// Handshake read timeout: a client that connects and says nothing.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing session waits for the client's close frame.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

type Socket = WebSocket<TcpStream>;

/// The listener, bound but not yet serving.
pub struct StreamServer {
    listener: TcpListener,
    addr: SocketAddr,
    host: ModelHost,
    tokens: Arc<StreamTokens>,
    observers: WorkerObservers,
}

impl StreamServer {
    pub fn bind(
        addr: &str,
        host: ModelHost,
        tokens: Arc<StreamTokens>,
        observers: WorkerObservers,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| anyhow::anyhow!("stream listener bind {addr}: {e}"))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        Ok(Self {
            listener,
            addr,
            host,
            tokens,
            observers,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Accept sessions until `stop` is set; one thread per session.
    pub fn serve(&self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let (host, tokens, observers) = (
                        self.host.clone(),
                        self.tokens.clone(),
                        self.observers.clone(),
                    );
                    std::thread::spawn(move || session(stream, peer, &host, &tokens, &observers));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL / 2)
                }
                Err(e) => {
                    tracing::warn!(target: TRACE_TARGET, op = "accept", error = %e, "stream accept failed");
                    std::thread::sleep(POLL);
                }
            }
        }
    }
}

fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn reject(status: u16, message: &str) -> ErrorResponse {
    let mut resp = ErrorResponse::new(Some(message.to_string()));
    *resp.status_mut() = tungstenite::http::StatusCode::from_u16(status)
        .unwrap_or(tungstenite::http::StatusCode::BAD_REQUEST);
    resp
}

/// Checks the path and the stream token during the WebSocket handshake,
/// remembering which model the token opens.
struct Handshake<'a> {
    tokens: &'a StreamTokens,
    model: &'a mut Option<String>,
}

impl Callback for Handshake<'_> {
    fn on_request(self, req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
        if req.uri().path() != STREAM_PATH {
            return Err(reject(404, "not found; stream to /transcribe"));
        }
        match query_param(req.uri().query(), "token").map(|t| self.tokens.check(t, Utc::now())) {
            Some(Ok(m)) => {
                *self.model = Some(m);
                Ok(resp)
            }
            Some(Err(rejection)) => Err(reject(401, &rejection.to_string())),
            None => Err(reject(401, "missing stream token")),
        }
    }
}

/// What a session did, for the log and the local-jobs ring.
#[derive(Default)]
struct Summary {
    audio_bytes: usize,
    final_text: Option<String>,
    error: Option<String>,
}

fn session(
    stream: TcpStream,
    peer: SocketAddr,
    host: &ModelHost,
    tokens: &StreamTokens,
    observers: &WorkerObservers,
) {
    let started = Instant::now();
    let started_at = Utc::now();
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err()
    {
        return;
    }
    let mut model: Option<String> = None;
    let handshake = Handshake {
        tokens,
        model: &mut model,
    };
    let mut ws = match tungstenite::accept_hdr(stream, handshake) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::info!(target: TRACE_TARGET, op = "handshake", %peer, error = %e, "stream refused");
            return;
        }
    };
    let Some(model) = model else { return };
    if ws.get_ref().set_read_timeout(Some(POLL)).is_err() {
        return;
    }
    tracing::info!(target: TRACE_TARGET, op = "stream", %peer, model = %model, "stream opened");
    match host.try_with_lane(&model, |loaded, lane| run(&mut ws, loaded, lane)) {
        Ok(summary) => {
            let outcome = match &summary.error {
                Some(reason) => JobOutcome::Failed {
                    reason: reason.clone(),
                },
                None => JobOutcome::Completed,
            };
            tracing::info!(
                target: TRACE_TARGET,
                op = "stream",
                %peer,
                model = %model,
                audio_ms = summary.audio_bytes / 32,
                final_chars = summary.final_text.as_ref().map_or(0, String::len),
                error = summary.error.as_deref().unwrap_or(""),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "stream closed"
            );
            record_local_job(
                observers,
                RecentJob {
                    job_id: crate::local::next_job_id(),
                    kind: TaskKind::AudioStt,
                    model,
                    prompt: truncate_prompt(summary.final_text.as_deref().unwrap_or("")),
                    outcome,
                    started_at,
                    finished_at: Utc::now(),
                },
            );
        }
        Err(err) => {
            tracing::info!(target: TRACE_TARGET, op = "stream", %peer, model = %model, error = %err, "stream refused");
            send(&mut ws, &ServerFrame::Error(err.to_string()));
            close(&mut ws);
        }
    }
}

/// Serve one session on a loaded model's lane.
fn run(ws: &mut Socket, loaded: &dyn LoadedModel, lane: &Lane) -> Summary {
    let summary = Summary::default();
    let Some(streaming) = loaded.as_stream() else {
        return fail(ws, summary, "model is not a streaming speech model".into());
    };
    let mut transcriber = match streaming.open() {
        Ok(t) => t,
        Err(e) => return fail(ws, summary, format!("could not open a stream: {e:#}")),
    };
    let mut session = StreamSession::new(transcriber.as_mut());
    let mut summary = summary;
    loop {
        if lane.cancelled() {
            return fail(ws, summary, "model unloaded".into());
        }
        let frame = match ws.read() {
            Ok(Message::Binary(bytes)) => {
                summary.audio_bytes += bytes.len();
                ClientFrame::Audio(bytes.to_vec())
            }
            Ok(Message::Text(text)) => match ClientFrame::from_text(&text) {
                Some(frame) => frame,
                None => {
                    send(
                        ws,
                        &ServerFrame::Error(format!(
                            "unknown frame {:?}; send audio, end or cancel",
                            text.as_str()
                        )),
                    );
                    continue;
                }
            },
            Ok(Message::Close(_)) => {
                summary.error = Some("client left before the final".into());
                return summary;
            }
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e)) if is_timeout(&e) => continue,
            Err(e) => {
                summary.error = Some(format!("stream broke: {e}"));
                return summary;
            }
        };
        let (frames, next) = session.handle(frame);
        for frame in &frames {
            match frame {
                ServerFrame::Final(text) => summary.final_text = Some(text.clone()),
                ServerFrame::Error(e) => summary.error = Some(e.clone()),
                ServerFrame::Partial(_) => {}
            }
            send(ws, frame);
        }
        if next == Next::Close {
            close(ws);
            return summary;
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn fail(ws: &mut Socket, mut summary: Summary, error: String) -> Summary {
    send(ws, &ServerFrame::Error(error.clone()));
    close(ws);
    summary.error = Some(error);
    summary
}

fn send(ws: &mut Socket, frame: &ServerFrame) {
    if let Err(e) = ws.send(Message::Text(frame.to_json().to_string().into())) {
        tracing::debug!(target: TRACE_TARGET, op = "send", error = %e, "stream send failed");
    }
}

/// Start the close handshake and give the client a moment to answer.
fn close(ws: &mut Socket) {
    let _ = ws.close(None);
    let deadline = Instant::now() + CLOSE_GRACE;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if is_timeout(&e) => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CatalogModel};
    use crate::host::ModelHost;
    use crate::lifecycle::ModelState;
    use crate::runtime::WorkerObservers;
    use crate::types::{ModelEngine, ModelSource, TaskKind};
    use chrono::Duration as ChronoDuration;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;
    use tungstenite::Message;

    const WAIT: Duration = Duration::from_secs(5);

    fn stt(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            kind: TaskKind::AudioStt,
            vram_gb_estimate: 1.0,
            description: None,
            source: ModelSource {
                engine: ModelEngine::Parakeet,
                files: vec![],
                cli_defaults: Default::default(),
            },
            enabled: true,
            origin: "local".into(),
            exclusive_group: None,
        }
    }

    struct Harness {
        host: ModelHost,
        tokens: Arc<StreamTokens>,
        observers: WorkerObservers,
        addr: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn start(loaded: bool) -> Harness {
        let catalog = Arc::new(Mutex::new(Catalog {
            models: vec![stt("stt-a")],
            ..Default::default()
        }));
        let host = ModelHost::new(
            catalog,
            Arc::new(crate::test_support::InstantRuntime),
            Arc::new(crate::test_support::FixedProbe(20.0)),
            crate::residency::Residency::load_for_serving(None),
        );
        if loaded {
            host.load("stt-a").unwrap();
            host.wait_for("stt-a", ModelState::serves, WAIT).unwrap();
        }
        let tokens = Arc::new(StreamTokens::default());
        let observers = WorkerObservers::default();
        let server = StreamServer::bind(
            "127.0.0.1:0",
            host.clone(),
            tokens.clone(),
            observers.clone(),
        )
        .unwrap();
        let addr = server.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || server.serve(&s));
        Harness {
            host,
            tokens,
            observers,
            addr,
            stop,
            handle: Some(handle),
        }
    }

    type Client = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

    /// Open a session, or the HTTP status the handshake was refused with.
    fn connect_to(h: &Harness, path: &str, token: &str) -> Result<Client, u16> {
        let url = format!("ws://{}{path}?token={token}", h.addr);
        match tungstenite::connect(url) {
            Ok((ws, _)) => Ok(ws),
            Err(tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
            Err(other) => panic!("unexpected connect error: {other}"),
        }
    }

    fn connect(h: &Harness, token: &str) -> Result<Client, u16> {
        connect_to(h, "/transcribe", token)
    }

    fn token(h: &Harness) -> String {
        h.tokens
            .mint("stt-a", ChronoDuration::minutes(5), chrono::Utc::now())
            .token
    }

    fn pcm(ms: usize, level: i16) -> Vec<u8> {
        (0..ms * 16)
            .flat_map(|i| (if i % 2 == 0 { level } else { -level }).to_le_bytes())
            .collect()
    }

    /// Every JSON text frame until the server closes.
    fn drain(ws: &mut Client) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => out.push(serde_json::from_str(&t).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return out,
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn a_session_streams_partials_then_the_final() {
        let h = start(true);
        let mut ws = connect(&h, &token(&h)).unwrap();
        ws.send(Message::Binary(pcm(200, 8000).into())).unwrap();
        ws.send(Message::Text("end".into())).unwrap();
        let frames = drain(&mut ws);
        assert_eq!(
            frames[0],
            serde_json::json!({ "partial": true, "text": "w1" })
        );
        assert_eq!(
            frames[1],
            serde_json::json!({ "partial": true, "text": "w1 w2" })
        );
        assert_eq!(
            frames.last().unwrap(),
            &serde_json::json!({ "final": true, "text": "w1 w2" })
        );
    }

    #[test]
    fn a_finished_session_is_recorded_as_a_local_job() {
        let h = start(true);
        let mut ws = connect(&h, &token(&h)).unwrap();
        ws.send(Message::Binary(pcm(100, 8000).into())).unwrap();
        ws.send(Message::Text("end".into())).unwrap();
        drain(&mut ws);
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            if let Some(job) = h.observers.local_jobs.lock().front().cloned() {
                assert_eq!(job.kind, TaskKind::AudioStt);
                assert_eq!(job.model, "stt-a");
                assert_eq!(job.prompt, "w1");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never recorded");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_unknown_token_is_refused_at_the_handshake() {
        let h = start(true);
        assert_eq!(connect(&h, "nope").err(), Some(401));
    }

    #[test]
    fn an_expired_token_is_refused_at_the_handshake() {
        let h = start(true);
        let old = h.tokens.mint(
            "stt-a",
            ChronoDuration::minutes(1),
            chrono::Utc::now() - ChronoDuration::hours(1),
        );
        assert_eq!(connect(&h, &old.token).err(), Some(401));
    }

    #[test]
    fn only_the_transcribe_path_is_served() {
        let h = start(true);
        assert_eq!(connect_to(&h, "/elsewhere", &token(&h)).err(), Some(404));
    }

    #[test]
    fn a_model_that_is_not_loaded_answers_with_an_error() {
        let h = start(false);
        let mut ws = connect(&h, &token(&h)).unwrap();
        let frames = drain(&mut ws);
        assert_eq!(
            frames,
            [serde_json::json!({ "error": "model stt-a is not loaded (unloaded)" })]
        );
    }

    #[test]
    fn a_second_stream_on_the_same_model_is_told_it_is_busy() {
        let h = start(true);
        let mut first = connect(&h, &token(&h)).unwrap();
        first.send(Message::Binary(pcm(100, 8000).into())).unwrap();
        // Wait until the first session is serving (its partial arrives).
        assert!(matches!(first.read(), Ok(Message::Text(_))));
        let mut second = connect(&h, &token(&h)).unwrap();
        let frames = drain(&mut second);
        assert_eq!(
            frames,
            [serde_json::json!({ "error": "model stt-a is busy serving another request" })]
        );
        first.send(Message::Text("cancel".into())).unwrap();
    }

    #[test]
    fn unloading_the_model_ends_the_stream() {
        let h = start(true);
        let mut ws = connect(&h, &token(&h)).unwrap();
        ws.send(Message::Binary(pcm(100, 8000).into())).unwrap();
        assert!(matches!(ws.read(), Ok(Message::Text(_))));
        h.host.unload("stt-a").unwrap();
        let frames = drain(&mut ws);
        assert_eq!(frames, [serde_json::json!({ "error": "model unloaded" })]);
        h.host
            .wait_for("stt-a", |s| *s == ModelState::Unloaded, WAIT)
            .unwrap();
    }

    #[test]
    fn an_unknown_text_frame_is_reported_and_the_session_continues() {
        let h = start(true);
        let mut ws = connect(&h, &token(&h)).unwrap();
        ws.send(Message::Text("hello?".into())).unwrap();
        ws.send(Message::Binary(pcm(100, 8000).into())).unwrap();
        ws.send(Message::Text("end".into())).unwrap();
        let frames = drain(&mut ws);
        assert_eq!(
            frames[0],
            serde_json::json!({ "error": "unknown frame \"hello?\"; send audio, end or cancel" })
        );
        assert_eq!(
            frames.last().unwrap(),
            &serde_json::json!({ "final": true, "text": "w1" })
        );
    }
}
