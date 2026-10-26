//! The operator's recordings end to end: a recording token minted on the loopback API, a take
//! uploaded with it over TLS to the LAN listener, the set listed and deleted on the loopback API.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;
use studio_worker::catalog::Catalog;
use studio_worker::engine::{Engine, SyntheticEngine};
use studio_worker::host::ModelHost;
use studio_worker::job_gate::JobGate;
use studio_worker::local_api::{LocalApi, ModelServices};
use studio_worker::runtime::WorkerObservers;
use studio_worker::stt_stream::server::{StreamCertificate, StreamServer};
use studio_worker::test_support::{FakeVisionRuntime, FixedProbe};
use studio_worker::voice_samples::VoiceSamples;

const TOKEN: &str = "voice-samples-test-token-0123456789";

struct Worker {
    api: String,
    listener: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    store: VoiceSamples,
    _dir: tempfile::TempDir,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn start(with_store: bool) -> Worker {
    start_with(with_store, true, true)
}

fn start_with(with_store: bool, listening: bool, on_a_lan: bool) -> Worker {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(Mutex::new(Catalog::default()));
    let host = ModelHost::new(
        catalog.clone(),
        Arc::new(FakeVisionRuntime),
        Arc::new(FixedProbe(20.0)),
        studio_worker::residency::Residency::load_for_serving(None),
    );
    let mut services = ModelServices::new(host.clone());
    services.lan_address = if on_a_lan {
        Arc::new(|| Ok(std::net::Ipv4Addr::new(192, 168, 1, 20)))
    } else {
        Arc::new(|| {
            studio_worker::stt_stream::server::lan_address(Ok(std::net::IpAddr::V4(
                std::net::Ipv4Addr::new(100, 64, 0, 1),
            )))
        })
    };
    let store = VoiceSamples::new(dir.path().join("voice-samples"));
    if with_store {
        services = services.with_voice_samples(store.clone());
    }
    let cert = StreamCertificate::load_or_create(dir.path()).unwrap();
    let mut server = StreamServer::bind(
        "127.0.0.1:0",
        &cert,
        host,
        services.tokens.clone(),
        WorkerObservers::default(),
    )
    .unwrap();
    if with_store {
        server = server.with_voice_samples(services.voice_tokens.clone(), store.clone());
    }
    if listening {
        *services.stream_endpoint.write() = Some(server.endpoint());
    }
    let listener = server.local_addr();
    let engine: Arc<dyn Engine> = Arc::new(SyntheticEngine::new());
    let api = LocalApi::bind(
        "127.0.0.1:0",
        engine,
        catalog,
        None,
        WorkerObservers::default(),
        TOKEN.into(),
        JobGate::new(),
        None,
        services,
    )
    .unwrap();
    let url = api.url();
    let stop = Arc::new(AtomicBool::new(false));
    let (s1, s2) = (stop.clone(), stop.clone());
    let threads = vec![
        std::thread::spawn(move || api.serve(&s1)),
        std::thread::spawn(move || server.serve(&s2)),
    ];
    Worker {
        api: url,
        listener,
        stop,
        threads,
        store,
        _dir: dir,
    }
}

fn call(w: &Worker, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = reqwest::blocking::Client::new()
        .request(method, format!("{}{path}", w.api))
        .bearer_auth(TOKEN);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().unwrap();
    (
        response.status().as_u16(),
        response.json().unwrap_or(Value::Null),
    )
}

/// Trusts exactly the certificate whose SHA-256 is the pin, as the phone does.
#[derive(Debug)]
struct Pin(String);

impl rustls::client::danger::ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest as _;
        let seen = hex::encode(sha2::Sha256::digest(end_entity.as_ref()));
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

fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// `PUT` a take as the grant says: its path under the grant's path, its token, its pinned certificate.
fn upload(w: &Worker, grant: &Value, take: &str, body: &[u8]) -> (u16, Value) {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin(grant["certSha256"]
            .as_str()
            .unwrap()
            .into())))
        .with_no_client_auth();
    let conn = rustls::ClientConnection::new(
        Arc::new(config),
        rustls::pki_types::ServerName::try_from("studio-worker").unwrap(),
    )
    .unwrap();
    let mut tls = rustls::StreamOwned::new(conn, TcpStream::connect(w.listener).unwrap());
    let path = format!("{}/{take}", grant["path"].as_str().unwrap());
    write!(
        tls,
        "PUT {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Bearer {}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
        grant["host"].as_str().unwrap(),
        grant["port"],
        grant["token"].as_str().unwrap(),
        body.len()
    )
    .unwrap();
    tls.write_all(body).unwrap();
    let mut answer = Vec::new();
    let _ = tls.read_to_end(&mut answer);
    let text = String::from_utf8(answer).unwrap();
    let status = text[9..12].parse().unwrap();
    (
        status,
        serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap(),
    )
}

/// A 16 kHz mono PCM16 WAV of `seconds` of silence.
fn wav(seconds: f64) -> Vec<u8> {
    let data = (seconds * 32_000.0) as u32;
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(16_000u32.to_le_bytes());
    out.extend(32_000u32.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    out.extend(std::iter::repeat_n(0u8, data as usize));
    out
}

#[test]
fn a_take_travels_from_grant_to_upload_to_list_to_delete() {
    let w = start(true);
    let (status, grant) = call(
        &w,
        reqwest::Method::POST,
        "/voice-sample-tokens",
        Some(serde_json::json!({"ttlSecs": 120})),
    );
    assert_eq!(status, 200, "{grant}");
    assert_eq!(grant["scheme"], "https");
    assert_eq!(grant["host"], "192.168.1.20");
    assert_eq!(grant["path"], "/voice-samples");
    assert_eq!(grant["port"].as_u64(), Some(u64::from(w.listener.port())));
    assert!(grant["certPem"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    assert_eq!(grant["token"].as_str().unwrap().len(), 64);

    for (take, seconds) in [
        ("positive/001.wav", 2.0),
        ("positive/002.wav", 2.0),
        ("near_miss/hey_luna-001.wav", 2.0),
        ("speech/001.wav", 30.0),
    ] {
        let (status, body) = upload(&w, &grant, &format!("2026-10-26/{take}"), &wav(seconds));
        assert_eq!(status, 201, "{take}: {body}");
    }

    let (status, list) = call(&w, reqwest::Method::GET, "/voice-samples", None);
    assert_eq!(status, 200);
    let set = &list["sets"][0];
    assert_eq!(set["set"], "2026-10-26");
    assert_eq!(
        (set["positive"].as_u64(), set["speech"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(set["nearMiss"]["hey_luna"], 1);
    assert_eq!(set["seconds"], 36.0);
    assert_eq!(set["inTraining"], false);

    let (status, body) = call(&w, reqwest::Method::DELETE, "/voice-samples/nope", None);
    assert_eq!((status, body["error"].as_str()), (404, Some("unknown_set")));
    let (status, body) = call(
        &w,
        reqwest::Method::DELETE,
        "/voice-samples/2026-10-26",
        None,
    );
    assert_eq!(
        (status, body["deleted"][0].as_str()),
        (200, Some("2026-10-26"))
    );
    let (_, list) = call(&w, reqwest::Method::GET, "/voice-samples", None);
    assert!(list["sets"].as_array().unwrap().is_empty());
    let (status, body) = call(&w, reqwest::Method::DELETE, "/voice-samples", None);
    assert_eq!(
        (status, body["deleted"].as_array().map(Vec::len)),
        (200, Some(0))
    );
}

#[test]
fn a_worker_without_a_store_says_so() {
    let w = start(false);
    let (status, body) = call(&w, reqwest::Method::POST, "/voice-sample-tokens", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("voice_samples_unavailable"))
    );
    let (status, body) = call(&w, reqwest::Method::GET, "/voice-samples", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("voice_samples_unavailable"))
    );
}

#[test]
fn bad_requests_are_refused_by_name() {
    let w = start(true);
    let (status, body) = call(
        &w,
        reqwest::Method::POST,
        "/voice-sample-tokens",
        Some(serde_json::json!({"ttl": 5})),
    );
    assert_eq!((status, body["error"].as_str()), (400, Some("bad_request")));
    let (status, body) = call(&w, reqwest::Method::GET, "/voice-sample-tokens", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (405, Some("method_not_allowed"))
    );
    let (status, body) = call(
        &w,
        reqwest::Method::DELETE,
        "/voice-samples/Not_A_Set",
        None,
    );
    assert_eq!((status, body["error"].as_str()), (400, Some("bad_name")));
    let (status, body) = call(&w, reqwest::Method::PUT, "/voice-samples", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (405, Some("method_not_allowed"))
    );
    let unauthenticated = reqwest::blocking::Client::new()
        .get(format!("{}/voice-samples", w.api))
        .send()
        .unwrap();
    assert_eq!(unauthenticated.status().as_u16(), 401);
}

#[test]
fn a_set_a_training_holds_is_not_deleted() {
    let w = start(true);
    let take = studio_worker::voice_samples::Take {
        kind: studio_worker::voice_samples::Kind::Positive,
        number: 1,
        phrase: None,
    };
    w.store.put("s1", &take, &wav(2.0)).unwrap();
    let held = w.store.hold(&["s1".into()]).unwrap();
    let (status, body) = call(&w, reqwest::Method::DELETE, "/voice-samples/s1", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("set_in_training"))
    );
    let (status, body) = call(&w, reqwest::Method::DELETE, "/voice-samples", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("set_in_training"))
    );
    let (_, list) = call(&w, reqwest::Method::GET, "/voice-samples", None);
    assert_eq!(list["sets"][0]["inTraining"], true);
    drop(held);
    let (status, _) = call(&w, reqwest::Method::DELETE, "/voice-samples", None);
    assert_eq!(status, 200);
}

#[test]
fn a_grant_says_when_there_is_no_listener_or_no_lan() {
    let w = start_with(true, false, true);
    let (status, body) = call(&w, reqwest::Method::POST, "/voice-sample-tokens", None);
    assert_eq!(
        (status, body["error"].as_str()),
        (503, Some("stream_listener_down"))
    );
    let w = start_with(true, true, false);
    let (status, grant) = call(
        &w,
        reqwest::Method::POST,
        "/voice-sample-tokens",
        Some(serde_json::json!({})),
    );
    assert_eq!(status, 200);
    assert!(grant["host"].is_null(), "no LAN address, no host: {grant}");
}
