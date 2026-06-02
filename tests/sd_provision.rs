//! Integration cover for `sd-cli` auto-provisioning
//! (`studio_worker::engine::sd_provision`).
//!
//! The real provisioner downloads the platform's stable-diffusion.cpp
//! release zip and extracts it; on a fresh box that's what makes image
//! generation work out of the box.  `provision` is `coverage(off)`
//! because it drives a real network download, so these tests prove the
//! end-to-end behaviour against a `wiremock` server serving a fake
//! release zip (no GPU, free-tier-CI safe):
//!
//! - the happy path downloads + extracts the zip and returns a runnable
//!   `<models_root>/bin/sd-cli` with the served bytes (and the exec bit
//!   on unix), then a second call short-circuits without re-downloading;
//! - a zip that doesn't contain the binary is surfaced as a clear error
//!   naming the missing file, with no half-installed binary left behind.
//!
//! `STUDIO_WORKER_SDCPP_URL` is process-global, so the two tests
//! serialise through a shared async mutex.

use std::io::Write;
use std::path::Path;
use studio_worker::engine::sd_provision;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path as match_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialises mutation of the process-global `STUDIO_WORKER_SDCPP_URL`.
static URL_ENV_LOCK: Mutex<()> = Mutex::const_new(());

const URL_ENV: &str = "STUDIO_WORKER_SDCPP_URL";

/// `reqwest::blocking` spins up its own runtime, which panics if dropped
/// inside an enclosing tokio context — run it on a detached OS thread.
fn detached<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(f)
        .join()
        .expect("worker thread panicked")
}

/// A minimal stand-in for an upstream release zip: the platform binary
/// plus a sibling shared library and a licence text, deflate-compressed
/// exactly like the real artefacts.
fn fake_release_zip(include_binary: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o755);
        if include_binary {
            zw.start_file(sd_provision::binary_name(), opts).unwrap();
            zw.write_all(b"#!/bin/sh\necho fake-sd-cli\n").unwrap();
        }
        let lib_opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("libstable-diffusion.so", lib_opts).unwrap();
        zw.write_all(b"pretend shared library").unwrap();
        zw.start_file("stable-diffusion.cpp.txt", lib_opts).unwrap();
        zw.write_all(b"MIT").unwrap();
        zw.finish().unwrap();
    }
    buf
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[tokio::test]
async fn provision_downloads_extracts_and_caches_sd_cli() {
    let _guard = URL_ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    // `expect(1)` proves the second `provision` call short-circuits on
    // the already-installed binary and never re-downloads.
    Mock::given(method("GET"))
        .and(match_path("/sd.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_release_zip(true)))
        .expect(1)
        .mount(&server)
        .await;
    let url = format!("{}/sd.zip", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let models_root = dir.path().to_path_buf();

    std::env::set_var(URL_ENV, &url);
    let root = models_root.clone();
    let first = detached(move || sd_provision::provision(&root).unwrap());
    std::env::remove_var(URL_ENV);

    let expected = models_root.join("bin").join(sd_provision::binary_name());
    assert_eq!(first, expected);
    assert!(first.is_file(), "binary must be installed");
    assert_eq!(
        std::fs::read(&first).unwrap(),
        b"#!/bin/sh\necho fake-sd-cli\n"
    );
    assert!(is_executable(&first), "binary must be executable");
    // The sibling library lands next to it so the loader can find it.
    assert!(models_root
        .join("bin")
        .join("libstable-diffusion.so")
        .is_file());
    // No scratch zip / staging dir litter left under models_root.
    let leftovers: Vec<_> = std::fs::read_dir(&models_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".sd-cli"))
        .collect();
    assert!(leftovers.is_empty(), "scratch litter left: {leftovers:?}");

    // Second call: binary already present -> no network (expect(1)).
    let root = models_root.clone();
    let second = detached(move || sd_provision::provision(&root).unwrap());
    assert_eq!(second, expected);
    // `server` drops here; wiremock asserts the single download.
}

#[tokio::test]
async fn provision_errors_when_zip_lacks_the_binary() {
    let _guard = URL_ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path("/broken.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_release_zip(false)))
        .mount(&server)
        .await;
    let url = format!("{}/broken.zip", server.uri());

    let dir = tempfile::tempdir().unwrap();
    let models_root = dir.path().to_path_buf();

    std::env::set_var(URL_ENV, &url);
    let root = models_root.clone();
    let err = detached(move || {
        sd_provision::provision(&root)
            .expect_err("a zip with no binary must error")
            .to_string()
    });
    std::env::remove_var(URL_ENV);

    assert!(err.contains(sd_provision::binary_name()), "got: {err}");
    // Nothing runnable installed.
    assert!(!models_root
        .join("bin")
        .join(sd_provision::binary_name())
        .exists());
}
