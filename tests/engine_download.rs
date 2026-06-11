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
//!
//! (Truncated-body rejection is covered by the `verify_download_len`
//! unit tests in the module; an HTTP server can't be made to under-send
//! a declared `Content-Length` — hyper enforces the two match.)

use studio_worker::engine::download;
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
