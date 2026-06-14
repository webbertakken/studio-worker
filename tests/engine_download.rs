//! Integration cover for the shared model downloader
//! (`studio_worker::engine::download`).
//!
//! The streaming download itself is marked `coverage(off)` because it
//! needs a real network + filesystem, so these tests drive it against a
//! `wiremock` server to prove the parts that actually keep a worker
//! self-provisioning safely:
//!
//! - the happy path writes the exact bytes the server served, and
//!   `ensure_file` returns that path;
//! - a non-2xx response is surfaced as an error (no file committed);
//! - an already-cached file is returned without touching the network.
//! - a body shorter than its declared `Content-Length` is rejected and
//!   leaves nothing cached (the integrity guard that stops a truncated
//!   model file from ever being loaded), driven end-to-end by a raw-TCP
//!   server that under-sends its declared length (see
//!   [`serve_truncated_response`] for why `wiremock` can't do this).

use std::io::{Read, Write};
use std::net::TcpListener;
use studio_worker::engine::download;
use studio_worker::test_support::capture as captured_logs_for;
use studio_worker::types::{ModelFile, ModelFileRole};
use wiremock::matchers::{method, path as match_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// sha256 of the b"a tiny pretend model" body used across these tests.
const BODY_SHA256: &str = "7820645b979bcfe59530fcd3b377c10e20bffed93396b8b3ffbd506f06aaacfe";

fn model_file(filename: &str, url: &str, sha256: Option<&str>) -> ModelFile {
    ModelFile {
        role: ModelFileRole::Model,
        url: url.to_string(),
        filename: filename.to_string(),
        approx_bytes: None,
        sha256: sha256.map(str::to_string),
    }
}

/// `reqwest::blocking` spins up its own runtime, which panics if dropped
/// inside an enclosing tokio context — run it on a detached OS thread.
fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

/// Spawn a one-shot raw HTTP/1.1 server that declares a large
/// `Content-Length` but sends only a few body bytes before closing the
/// socket, so the client observes a genuinely *truncated* body.
///
/// This is deliberately raw TCP rather than `wiremock`: a
/// `wiremock`/`hyper` response whose body length disagrees with its
/// declared `Content-Length` makes the **server** panic while framing
/// the reply, so the client only ever sees a generic connection error —
/// the download integrity guard (the streaming-read failure /
/// `verify_download_len`) is never reached, and the test passes for the
/// wrong reason.  Writing the malformed response by hand bypasses
/// hyper's framing validation and exercises the real guard.  Returns
/// the server's base URL (`http://127.0.0.1:<port>`).
fn serve_truncated_response() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Drain the request line + headers so the client's write
            // side completes before we reply (a GET fits in one read).
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // Declare 9999 bytes but send 16, then drop the socket: the
            // client reads a short body then hits EOF mid-message.
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9999\r\n\r\nonly a few bytes");
            let _ = stream.flush();
            // `stream` drops here, closing the connection mid-body.
        }
    });
    format!("http://{addr}")
}

/// Grab a loopback port, then drop the listener so nothing is bound to
/// it.  A subsequent connect to that address is refused at the TCP
/// level (`ECONNREFUSED`), which is how the most common real-world
/// download failures surface: a flaky network, a closed firewall port,
/// a DNS hiccup, or a TLS handshake error all bubble up as a
/// connection-level `reqwest::send()` error *before* any HTTP status is
/// seen.  Returns a URL on that now-closed port.
fn refused_loopback_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    // Free the port so the connect that follows is refused, not served.
    drop(listener);
    format!("http://{addr}/model.gguf")
}

#[tokio::test]
async fn download_file_writes_the_served_bytes() {
    let server = MockServer::start().await;
    let body = b"a tiny pretend model".to_vec();
    Mock::given(method("GET"))
        .and(match_path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    let url = format!("{}/model.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("model.gguf");
    let dest_for_thread = dest.clone();
    detached(move || download::download_file(&url, &dest_for_thread).unwrap());

    assert_eq!(std::fs::read(&dest).unwrap(), body);
    // No `.part` litter left behind.
    assert!(!dest.with_extension("part").exists());
}

#[tokio::test]
async fn ensure_file_accepts_a_matching_sha256() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/verified.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"a tiny pretend model".to_vec()))
        .mount(&server)
        .await;
    let url = format!("{}/verified.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let file = model_file("verified.gguf", &url, Some(BODY_SHA256));
    let local = detached(move || download::ensure_file(&dir_path, &file).unwrap());
    assert!(local.is_file());
}

#[tokio::test]
async fn ensure_file_rejects_a_sha256_mismatch_and_caches_nothing() {
    // A corrupted or tampered body must never be renamed into the
    // cache — every later job would fail loading it (or worse, run it).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/tampered.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"evil bytes".to_vec()))
        .mount(&server)
        .await;
    let url = format!("{}/tampered.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let file = model_file("tampered.gguf", &url, Some(BODY_SHA256));
    let err = detached(move || download::ensure_file(&dir_path, &file).unwrap_err());
    assert!(
        err.to_string().contains("sha256") || format!("{err:#}").contains("sha256"),
        "error must name the sha256 mismatch: {err:#}"
    );
    assert!(!dir.path().join("tampered.gguf").exists());
    assert!(!dir.path().join("tampered.part").exists());
}

#[tokio::test]
async fn ensure_file_is_case_insensitive_about_the_expected_hash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/upper.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"a tiny pretend model".to_vec()))
        .mount(&server)
        .await;
    let url = format!("{}/upper.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let file = model_file("upper.gguf", &url, Some(&BODY_SHA256.to_uppercase()));
    let local = detached(move || download::ensure_file(&dir_path, &file).unwrap());
    assert!(local.is_file());
}

#[tokio::test]
async fn download_file_surfaces_a_non_success_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/missing.gguf"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let url = format!("{}/missing.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("missing.gguf");
    let dest_for_thread = dest.clone();
    let err = detached(move || {
        download::download_file(&url, &dest_for_thread)
            .expect_err("404 must error")
            .to_string()
    });
    assert!(err.contains("404"), "got: {err}");
    assert!(!dest.exists());
}

#[test]
fn download_surfaces_a_connection_level_failure_and_caches_nothing() {
    // A connection that never reaches an HTTP response (refused,
    // DNS/TLS error, dropped before the status line) must surface as an
    // error and commit nothing — the symmetric sibling of the 404 and
    // truncation guards.  Without this the most common production
    // failure (a network blip) was the only download error path with no
    // regression cover.
    let url = refused_loopback_url();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("refused.gguf");
    let dest_for_thread = dest.clone();
    let err = detached(move || {
        download::download_file(&url, &dest_for_thread)
            .expect_err("a refused connection must error")
            .to_string()
    });
    // The download path attaches a `GET` context to a connection-level
    // send() failure, distinguishing it from a non-success status
    // (which bails with `GET <url> -> <status>`).
    assert_eq!(
        err, "GET",
        "expected the connection-level GET context, got: {err}"
    );
    assert!(!dest.exists(), "no file committed on a refused connection");
    assert!(
        !dest.with_extension("part").exists(),
        ".part scratch litter left behind"
    );
}

#[test]
fn download_rejects_a_truncated_body_and_caches_nothing() {
    // A body shorter than its declared Content-Length must never be
    // renamed into the cache — a half-written GGUF would crash every
    // later job that loads it (or worse, be run).  The raw-TCP server
    // under-sends its declared length so the streaming read fails
    // mid-body and the integrity guard rejects it.
    let url = format!("{}/truncated.gguf", serve_truncated_response());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("truncated.gguf");
    let dest_for_thread = dest.clone();
    let err = detached(move || {
        download::download_file(&url, &dest_for_thread)
            .expect_err("a truncated download must be rejected")
            .to_string()
    });
    // The failure must come from the body-streaming guard, not an
    // unrelated connection error: a truncated read surfaces as the
    // "streaming body" context the download path attaches.
    assert!(
        err.contains("streaming body"),
        "expected the streaming-read integrity guard to fire, got: {err}"
    );
    // No file and no `.part` litter may survive.
    assert!(!dest.exists(), "no truncated file may be committed");
    assert!(
        !dest.with_extension("part").exists(),
        ".part scratch litter left behind"
    );
}

#[test]
fn truncated_download_emits_a_warn_breadcrumb() {
    // The integrity guard must also be observable: an operator filtering
    // `studio_worker::engine::download` needs a terminal `warn` after the
    // `info` "starting" line, not silence.
    let url = format!("{}/short.gguf", serve_truncated_response());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("short.gguf");
    let dest_for_thread = dest.clone();
    let logs = captured_logs_for(move || {
        let _ = download::download_file(&url, &dest_for_thread);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"download\""),
        "expected op field, got: {logs}"
    );
    // The breadcrumb must name the streaming-read failure specifically,
    // proving the truncation guard fired rather than a generic GET error.
    assert!(
        logs.contains("download failed: streaming body"),
        "expected the streaming-body failure breadcrumb, got: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Tracing emission — a failed model download must leave an
// operator-visible `warn` breadcrumb at the download target, mirroring
// the `ApiClient` HTTP surface.  Without it, an operator filtering
// `studio_worker::engine::download` sees the `info` "starting" line
// then silence, with no terminal event explaining what went wrong.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_failure_emits_a_warn_breadcrumb_with_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/gone.gguf"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let url = format!("{}/gone.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("gone.gguf");
    let dest_for_thread = dest.clone();
    // `capture` spawns its own non-tokio thread, so `reqwest::blocking`
    // inside the closure works without a separate `detached(...)` wrap.
    let logs = captured_logs_for(move || {
        let _ = download::download_file(&url, &dest_for_thread);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"download\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("status=404"),
        "expected status field, got: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field, got: {logs}"
    );
    assert!(
        logs.contains("gone.gguf"),
        "expected the dest/url in the breadcrumb, got: {logs}"
    );
    assert!(!dest.exists(), "no file committed on a failed download");
}

#[test]
fn connection_level_failure_emits_a_warn_breadcrumb() {
    // The connection-level failure path must be observable too: an
    // operator filtering `studio_worker::engine::download` needs a
    // terminal `warn` after the `info` "starting" line, not silence.
    // This breadcrumb names "request error" specifically, which is
    // unique to the `send()` Err arm — proving the connection guard
    // fired rather than a status or streaming failure.
    let url = refused_loopback_url();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("refused.gguf");
    let dest_for_thread = dest.clone();
    let logs = captured_logs_for(move || {
        let _ = download::download_file(&url, &dest_for_thread);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"download\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("download failed: request error"),
        "expected the connection-level failure breadcrumb, got: {logs}"
    );
    assert!(
        logs.contains("elapsed_ms"),
        "expected elapsed_ms field, got: {logs}"
    );
    assert!(!dest.exists(), "no file committed on a refused connection");
}

#[tokio::test]
async fn sha256_mismatch_emits_a_warn_breadcrumb() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/tampered.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"evil bytes".to_vec()))
        .mount(&server)
        .await;
    let url = format!("{}/tampered.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let file = model_file("tampered.gguf", &url, Some(BODY_SHA256));
    let logs = captured_logs_for(move || {
        let _ = download::ensure_file(&dir_path, &file);
    });

    assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
    assert!(
        logs.contains("op=\"download\""),
        "expected op field, got: {logs}"
    );
    assert!(
        logs.contains("sha256"),
        "expected the sha256 reason in the breadcrumb, got: {logs}"
    );
}

#[tokio::test]
async fn ensure_file_downloads_when_missing_then_reuses_the_cache() {
    let server = MockServer::start().await;
    let body = b"cached model bytes".to_vec();
    // `expect(1)` proves the second `ensure_file` call does NOT hit the
    // network — the cached file is reused.
    Mock::given(method("GET"))
        .and(match_path("/once.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let url = format!("{}/once.gguf", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let file_for_thread = model_file("once.gguf", &url, None);
    let first = detached(move || download::ensure_file(&dir_path, &file_for_thread).unwrap());
    assert_eq!(std::fs::read(&first).unwrap(), body);

    let dir_path = dir.path().to_path_buf();
    let file = model_file("once.gguf", &url, None);
    let second = detached(move || download::ensure_file(&dir_path, &file).unwrap());
    assert_eq!(first, second);
    // `server` drops here; wiremock asserts the `expect(1)` was met.
}
