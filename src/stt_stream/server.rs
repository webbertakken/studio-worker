//! The LAN streaming listener: `wss://<host>:<port>/transcribe`, with the
//! stream token in the handshake's `Authorization: Bearer` header.
//! A stream token (minted on the loopback API) names the model; the
//! session runs on that loaded model's lane until it finalises, the
//! client leaves, or the model is unloaded.
//!
//! Binds the LAN on purpose (a phone streams to it), over TLS only: the
//! listener presents its pinned certificate ([`super::tls`]), so neither the
//! audio nor the token crosses the LAN in the clear; the stream token is the
//! guard.  One session per model at a time (`try_with_lane`).

use super::session::{ClientFrame, Endpointing, Next, ServerFrame, StreamSession};
pub use super::tls::StreamCertificate;
pub use super::tokens::StreamTokens;
use crate::host::{Lane, LoadedModel, ModelHost};
use crate::job_run::JobRun;
use crate::runtime::{CurrentJob, JobOutcome, JobSource, WorkerObservers};
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
/// The one scheme served: WebSocket over TLS.
pub const STREAM_SCHEME: &str = "wss";
const TRACE_TARGET: &str = "studio_worker::stt_stream";
/// Read timeout while streaming: how often an idle session checks for an
/// unload.  Safe range 20..=500 ms.
const POLL: Duration = Duration::from_millis(100);
/// Handshake read timeout: a client that connects and says nothing.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing session waits for the client's close frame.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

type Tls = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;
type Socket = WebSocket<Tls>;

/// Where a client streams: the bound port and the certificate it pins.
/// Public; the loopback API hands it out with every stream grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEndpoint {
    pub port: u16,
    /// Lowercase hex SHA-256 of the certificate's DER.
    pub cert_sha256: String,
    pub cert_pem: String,
}

/// The listener, bound but not yet serving.
pub struct StreamServer {
    listener: TcpListener,
    addr: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    cert_sha256: String,
    cert_pem: String,
    host: ModelHost,
    tokens: Arc<StreamTokens>,
    observers: WorkerObservers,
}

impl StreamServer {
    /// Bind [addr], presenting [cert] to every client.
    pub fn bind(
        addr: &str,
        cert: &StreamCertificate,
        host: ModelHost,
        tokens: Arc<StreamTokens>,
        observers: WorkerObservers,
    ) -> anyhow::Result<Self> {
        let tls = cert.server_config()?;
        let listener = TcpListener::bind(addr)
            .map_err(|e| anyhow::anyhow!("stream listener bind {addr}: {e}"))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        Ok(Self {
            listener,
            addr,
            tls,
            cert_sha256: cert.sha256().to_owned(),
            cert_pem: cert.pem().to_owned(),
            host,
            tokens,
            observers,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn endpoint(&self) -> StreamEndpoint {
        StreamEndpoint {
            port: self.addr.port(),
            cert_sha256: self.cert_sha256.clone(),
            cert_pem: self.cert_pem.clone(),
        }
    }

    /// Accept sessions until `stop` is set; one thread per session.
    pub fn serve(&self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let (tls, host, tokens, observers) = (
                        self.tls.clone(),
                        self.host.clone(),
                        self.tokens.clone(),
                        self.observers.clone(),
                    );
                    std::thread::spawn(move || {
                        session(stream, peer, tls, &host, &tokens, &observers)
                    });
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

/// The token in an `Authorization: Bearer <token>` header (scheme in any case).
fn bearer(req: &Request) -> Option<&str> {
    let value = req
        .headers()
        .get(tungstenite::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Checks the path and the stream token during the WebSocket handshake,
/// remembering which model the token opens.
struct Handshake<'a> {
    tokens: &'a StreamTokens,
    model: &'a mut Option<String>,
    endpointing: &'a mut Endpointing,
}

impl Callback for Handshake<'_> {
    fn on_request(self, req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
        if req.uri().path() != STREAM_PATH {
            return Err(reject(404, "not found; stream to /transcribe"));
        }
        if query_param(req.uri().query(), "token").is_some() {
            return Err(reject(
                400,
                "a stream token in the URL is refused; send Authorization: Bearer <token>",
            ));
        }
        let Some(endpointing) = Endpointing::parse(query_param(req.uri().query(), "endpoint"))
        else {
            return Err(reject(400, "endpoint must be server or client"));
        };
        *self.endpointing = endpointing;
        match bearer(req).map(|t| self.tokens.check(t, Utc::now())) {
            Some(Ok(m)) => {
                *self.model = Some(m);
                Ok(resp)
            }
            Some(Err(rejection)) => Err(reject(401, &rejection.to_string())),
            None => Err(reject(
                401,
                "missing stream token; send Authorization: Bearer <token>",
            )),
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

/// The TLS handshake, driven to completion before any HTTP is read, so a
/// plain-text client is refused here and nothing it sent is parsed.
fn accept_tls(
    mut stream: TcpStream,
    tls: Arc<rustls::ServerConfig>,
) -> Result<Tls, std::io::Error> {
    let mut conn = rustls::ServerConnection::new(tls).map_err(std::io::Error::other)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut stream)?;
    }
    Ok(rustls::StreamOwned::new(conn, stream))
}

fn session(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
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
    let stream = match accept_tls(stream, tls) {
        Ok(stream) => stream,
        Err(e) => {
            tracing::info!(target: TRACE_TARGET, op = "tls", %peer, error = %e, "stream refused");
            return;
        }
    };
    let mut model: Option<String> = None;
    let mut endpointing = Endpointing::Server;
    let handshake = Handshake {
        tokens,
        model: &mut model,
        endpointing: &mut endpointing,
    };
    let mut ws = match tungstenite::accept_hdr(stream, handshake) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::info!(target: TRACE_TARGET, op = "handshake", %peer, error = %e, "stream refused");
            return;
        }
    };
    let Some(model) = model else { return };
    if ws.get_ref().sock.set_read_timeout(Some(POLL)).is_err() {
        return;
    }
    tracing::info!(target: TRACE_TARGET, op = "stream", %peer, model = %model, ?endpointing, "stream opened");
    let served = host.try_with_lane(&model, |loaded, lane| {
        let job = JobRun::begin(
            observers,
            CurrentJob {
                job_id: crate::local::next_job_id(),
                kind: TaskKind::AudioStt,
                model: model.clone(),
                prompt: String::new(),
                started_at,
                source: JobSource::Stream,
            },
        );
        let summary = job
            .span()
            .in_scope(|| run(&mut ws, loaded, lane, endpointing));
        (job, summary)
    });
    match served {
        Ok((mut job, summary)) => {
            let outcome = match &summary.error {
                Some(reason) => JobOutcome::Failed {
                    reason: reason.clone(),
                },
                None => JobOutcome::Completed,
            };
            job.span().in_scope(|| {
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
            });
            job.set_prompt(summary.final_text.as_deref().unwrap_or(""));
            job.finish(outcome);
        }
        Err(err) => {
            tracing::info!(target: TRACE_TARGET, op = "stream", %peer, model = %model, error = %err, "stream refused");
            send(&mut ws, &ServerFrame::Error(err.to_string()));
            close(&mut ws);
        }
    }
}

/// Serve one session on a loaded model's lane.
fn run(
    ws: &mut Socket,
    loaded: &dyn LoadedModel,
    lane: &Lane,
    endpointing: Endpointing,
) -> Summary {
    let summary = Summary::default();
    let Some(streaming) = loaded.as_stream() else {
        return fail(ws, summary, "model is not a streaming speech model".into());
    };
    let mut transcriber = match streaming.open() {
        Ok(t) => t,
        Err(e) => return fail(ws, summary, format!("could not open a stream: {e:#}")),
    };
    let mut session = StreamSession::with_endpointing(transcriber.as_mut(), endpointing);
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
        endpoint: StreamEndpoint,
        /// The certificate the listener presents; dropped with the harness.
        _cert_dir: tempfile::TempDir,
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
        let cert_dir = tempfile::tempdir().unwrap();
        let cert = StreamCertificate::load_or_create(cert_dir.path()).unwrap();
        let server = StreamServer::bind(
            "127.0.0.1:0",
            &cert,
            host.clone(),
            tokens.clone(),
            observers.clone(),
        )
        .unwrap();
        let addr = server.local_addr();
        let endpoint = server.endpoint();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || server.serve(&s));
        Harness {
            host,
            tokens,
            observers,
            addr,
            endpoint,
            _cert_dir: cert_dir,
            stop,
            handle: Some(handle),
        }
    }

    type Client = tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

    /// How a client was turned away.
    #[derive(Debug, PartialEq, Eq)]
    enum Refused {
        /// The WebSocket handshake answered this HTTP status.
        Status(u16),
        /// The TLS handshake failed: nothing was sent.
        Tls,
    }

    /// Trusts exactly the certificate whose SHA-256 is the pin, as the app does.
    #[derive(Debug)]
    struct Pin(String);

    fn provider() -> rustls::crypto::CryptoProvider {
        rustls::crypto::ring::default_provider()
    }

    impl rustls::client::danger::ServerCertVerifier for Pin {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            use sha2::Digest as _;
            let seen: String = sha2::Sha256::digest(end_entity.as_ref())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if seen == self.0 {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General(
                    "certificate does not match the pin".into(),
                ))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Open `wss://<listener><target>` trusting only [pin], with [token] as
    /// the bearer header; a session, or how it was refused.
    fn open(h: &Harness, target: &str, token: Option<&str>, pin: &str) -> Result<Client, Refused> {
        use tungstenite::client::IntoClientRequest as _;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(pin.into())))
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("studio-worker").unwrap(),
        )
        .unwrap();
        let tls = rustls::StreamOwned::new(conn, TcpStream::connect(h.addr).unwrap());
        let mut request = format!("wss://{}{target}", h.addr)
            .into_client_request()
            .unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        match tungstenite::client(request, tls) {
            Ok((ws, _)) => Ok(ws),
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
                Err(Refused::Status(resp.status().as_u16()))
            }
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Io(e)))
                if e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) =>
            {
                Err(Refused::Tls)
            }
            Err(other) => panic!("unexpected connect error: {other}"),
        }
    }

    /// Open a session on [path] with a header token, trusting the listener's pin.
    fn connect_to(h: &Harness, path: &str, token: &str) -> Result<Client, Refused> {
        open(h, path, Some(token), &h.endpoint.cert_sha256)
    }

    fn connect(h: &Harness, token: &str) -> Result<Client, Refused> {
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
    fn client_endpointing_holds_the_session_through_a_pause() {
        let h = start(true);
        let t = token(&h);
        let mut ws = connect_to(&h, "/transcribe?endpoint=client", &t).unwrap();
        ws.send(Message::Binary(pcm(400, 8000).into())).unwrap();
        ws.send(Message::Binary(pcm(2000, 0).into())).unwrap();
        ws.send(Message::Binary(pcm(400, 8000).into())).unwrap();
        ws.send(Message::Text("end".into())).unwrap();
        let frames = drain(&mut ws);
        let finals: Vec<_> = frames.iter().filter(|f| f["final"] == true).collect();
        assert_eq!(finals.len(), 1, "one final, only after end: {frames:?}");
        assert_eq!(frames.last().unwrap()["final"], true);
    }

    #[test]
    fn server_endpointing_still_finalises_on_a_pause() {
        let h = start(true);
        let t = token(&h);
        let mut ws = connect_to(&h, "/transcribe?endpoint=server", &t).unwrap();
        ws.send(Message::Binary(pcm(400, 8000).into())).unwrap();
        ws.send(Message::Binary(pcm(2000, 0).into())).unwrap();
        let frames = drain(&mut ws);
        assert_eq!(frames.last().unwrap()["final"], true);
    }

    #[test]
    fn an_unknown_endpointing_is_refused_at_the_handshake() {
        let h = start(true);
        let t = token(&h);
        assert_eq!(
            connect_to(&h, "/transcribe?endpoint=both", &t).err(),
            Some(Refused::Status(400))
        );
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
                assert_eq!(job.source, crate::runtime::JobSource::Stream);
                assert!(h.observers.active_jobs.lock().is_empty());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never recorded");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_unknown_token_is_refused_at_the_handshake() {
        let h = start(true);
        assert_eq!(connect(&h, "nope").err(), Some(Refused::Status(401)));
    }

    #[test]
    fn an_expired_token_is_refused_at_the_handshake() {
        let h = start(true);
        let old = h.tokens.mint(
            "stt-a",
            ChronoDuration::minutes(1),
            chrono::Utc::now() - ChronoDuration::hours(1),
        );
        assert_eq!(connect(&h, &old.token).err(), Some(Refused::Status(401)));
    }

    #[test]
    fn only_the_transcribe_path_is_served() {
        let h = start(true);
        assert_eq!(
            connect_to(&h, "/elsewhere", &token(&h)).err(),
            Some(Refused::Status(404))
        );
    }

    #[test]
    fn the_endpoint_names_the_port_and_the_pinned_certificate() {
        let h = start(true);
        assert_eq!(h.endpoint.port, h.addr.port());
        assert_eq!(h.endpoint.cert_sha256.len(), 64);
        assert!(h
            .endpoint
            .cert_pem
            .starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn a_missing_token_is_refused_at_the_handshake() {
        let h = start(true);
        assert_eq!(
            open(&h, "/transcribe", None, &h.endpoint.cert_sha256).err(),
            Some(Refused::Status(401))
        );
    }

    #[test]
    fn a_token_in_the_query_is_refused_even_when_valid() {
        let h = start(true);
        let t = token(&h);
        let pin = h.endpoint.cert_sha256.clone();
        let target = format!("/transcribe?token={t}");
        assert_eq!(
            open(&h, &target, None, &pin).err(),
            Some(Refused::Status(400))
        );
        assert_eq!(
            open(&h, &target, Some(&t), &pin).err(),
            Some(Refused::Status(400)),
            "a header does not excuse a token in the URL"
        );
    }

    #[test]
    fn a_wrong_pin_fails_the_tls_handshake_before_the_token_is_sent() {
        let h = start(true);
        let t = token(&h);
        assert_eq!(
            open(&h, "/transcribe", Some(&t), &"0".repeat(64)).err(),
            Some(Refused::Tls)
        );
        assert!(h.observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn plain_websocket_is_refused() {
        let h = start(true);
        let t = token(&h);
        let mut request = tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{}/transcribe",
            h.addr
        ))
        .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
        let plain = TcpStream::connect(h.addr).unwrap();
        plain.set_read_timeout(Some(WAIT)).unwrap();
        assert!(tungstenite::client(request, plain).is_err());
        assert!(h.observers.local_jobs.lock().is_empty());
        assert!(connect(&h, &t).is_ok(), "a TLS client still gets in");
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
