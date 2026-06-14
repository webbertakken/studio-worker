//! Shared model-file provisioning used by every real engine.
//!
//! The studio attaches a [`ModelSource`](crate::types::ModelSource) to
//! each real offer listing the files the worker needs (diffusion model,
//! GGUF, VAE, ...) with a public URL + filename each.  Engines fetch
//! them on first use and cache them under their per-engine directory, so
//! a fresh worker provisions itself with no manual model placement.
//!
//! The streamed body is checked against the server's `Content-Length`,
//! so a truncated download is rejected and cleaned up instead of being
//! renamed into place as a corrupt model that every later job fails to
//! load.
//!
//! Every download emits a structured `tracing` breadcrumb at the
//! `studio_worker::engine::download` target: `info` on `starting` and
//! `done`, and a symmetric `warn` on each failure (non-success status,
//! a streaming error, or a length / sha256 mismatch) so an operator
//! never sees a dangling `starting` with no terminal event explaining
//! what went wrong — mirroring the `ApiClient` HTTP surface.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;
use tracing::{info, warn};

use crate::types::ModelFile;

/// Tracing target for model downloads.  Stable so operators can filter
/// with `RUST_LOG=studio_worker::engine::download=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::download";

/// HTTP client timeout per request — a GGUF / safetensors file is up to
/// a few GiB so a 30-minute ceiling is generous.
const DOWNLOAD_TIMEOUT_SECS: u64 = 30 * 60;

/// Resolve `filename` to a path inside `dir`, refusing anything that
/// is not a plain file name (no `/`, `\`, `..`, or absolute paths) so a
/// malicious or buggy `ModelSource` can't write outside the cache.
pub fn model_cache_path(dir: &Path, filename: &str) -> Result<PathBuf> {
    let path = Path::new(filename);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None)
            if !filename.contains('/') && !filename.contains('\\') =>
        {
            Ok(dir.join(name))
        }
        _ => bail!("model filename must be a plain file name: {filename:?}"),
    }
}

/// Verify a streamed download wrote exactly the body the server
/// promised.  `expected` is the response's `Content-Length`; it is
/// `None` for chunked transfers, where there's nothing to check and we
/// accept whatever arrived.  A mismatch in either direction means the
/// download is truncated or corrupt, so we surface a clear error rather
/// than cache a bad model.
pub fn verify_download_len(copied: u64, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if copied != expected => bail!(
            "size mismatch: wrote {copied} bytes but the server declared \
             Content-Length {expected} (download truncated or corrupt)"
        ),
        _ => Ok(()),
    }
}

/// Verify a downloaded body's sha256 against the registry's expected
/// hex digest (case-insensitive).  `None` means the registry row
/// predates integrity hashes — nothing to check.  A mismatch means a
/// corrupted or tampered body that must never be committed to the
/// cache.
pub fn verify_sha256(actual_hex: &str, expected: Option<&str>) -> Result<()> {
    match expected {
        Some(expected) if !actual_hex.eq_ignore_ascii_case(expected.trim()) => bail!(
            "sha256 mismatch: downloaded body hashes to {actual_hex} but the registry \
             expects {expected} (corrupted or tampered download)"
        ),
        _ => Ok(()),
    }
}

/// Writer adapter that feeds every chunk through a [`Sha256`] hasher
/// on its way to the underlying file, so verification needs no second
/// read pass over a multi-GiB model.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Best-effort removal of a temporary file — a partial `.part`
/// download, an engine's per-job scratch image, or a downloaded init /
/// mask.  A `NotFound` is the desired end state (something already
/// cleaned it up); any other failure is surfaced so a stuck temp file
/// can't silently fill the worker's disk over a long session.
pub fn remove_temp_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                target: TRACE_TARGET,
                op = "cleanup",
                path = %path.display(),
                error = %e,
                "failed to remove temp file"
            );
        }
    }
}

/// RAII owner of a job's scratch files.  Registering a job's temp
/// paths up front means every exit path — the success return, an
/// engine error, even a panic mid-dispatch — removes them on drop
/// instead of leaking them into the temp dir and slowly filling the
/// worker's disk over a long-running session.  Removal is best-effort
/// via [`remove_temp_file`], so a path that never materialised (the
/// job failed before the file was written) is silently tolerated.
#[derive(Default)]
pub struct TempFileGuard {
    paths: Vec<PathBuf>,
}

impl TempFileGuard {
    pub fn new() -> Self {
        Self { paths: Vec::new() }
    }

    /// Register a path to be removed when the guard drops.
    pub fn push(&mut self, path: PathBuf) {
        self.paths.push(path);
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        for path in &self.paths {
            remove_temp_file(path);
        }
    }
}

/// Ensure `file.filename` is present under `dir`, downloading it from
/// `file.url` when missing (verified against `file.sha256` when the
/// registry provides one).  Returns the resolved local path.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn ensure_file(dir: &Path, file: &ModelFile) -> Result<PathBuf> {
    let filename = file.filename.as_str();
    let url = file.url.as_str();
    let local = model_cache_path(dir, filename)?;
    if local.is_file() {
        tracing::debug!(
            target: TRACE_TARGET,
            op = "ensure_file",
            filename,
            path = %local.display(),
            "cached"
        );
        return Ok(local);
    }
    download_file_verified(url, &local, file.sha256.as_deref())
        .with_context(|| format!("downloading {filename} ({url}) -> {}", local.display()))?;
    Ok(local)
}

/// Stream `url` into `dest` (atomic via a `.part` rename so a killed
/// download doesn't leave a half-written file on disk).
///
/// Excluded from coverage: requires real network + filesystem (and a
/// multi-GiB download per model on the happy path).  Exercised
/// end-to-end via the live dev loop; the pure guards
/// ([`verify_download_len`], [`model_cache_path`]) are unit-tested.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    download_file_verified(url, dest, None)
}

/// [`download_file`] with an optional expected sha256 — the body is
/// hashed while it streams and a mismatch is rejected before the
/// rename, so a bad body never lands in the cache.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn download_file_verified(url: &str, dest: &Path, expected_sha256: Option<&str>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = dest.with_extension("part");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
        .build()?;
    info!(
        target: TRACE_TARGET,
        op = "download",
        url,
        dest = %dest.display(),
        "starting"
    );
    let started = Instant::now();
    let mut response = client.get(url).send().context("GET")?;
    let status = response.status();
    if !status.is_success() {
        warn!(
            target: TRACE_TARGET,
            op = "download",
            url,
            dest = %dest.display(),
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "download failed: non-success status"
        );
        bail!("GET {url} -> {status}");
    }
    let expected_len = response.content_length();
    let file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut writer = HashingWriter {
        inner: file,
        hasher: Sha256::new(),
    };
    let copied = std::io::copy(&mut response, &mut writer);
    let digest = writer.hasher.finalize();
    // Close the handle before any remove / rename so cleanup works on
    // Windows, where an open file can't be unlinked.
    drop(writer.inner);
    let bytes = match copied {
        Ok(bytes) => bytes,
        Err(e) => {
            remove_temp_file(&part);
            warn!(
                target: TRACE_TARGET,
                op = "download",
                url,
                dest = %dest.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "download failed: streaming body"
            );
            return Err(e).context("streaming body");
        }
    };
    if let Err(e) = verify_download_len(bytes, expected_len) {
        remove_temp_file(&part);
        warn!(
            target: TRACE_TARGET,
            op = "download",
            url,
            dest = %dest.display(),
            bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %e,
            "download failed: size mismatch"
        );
        return Err(e).with_context(|| format!("downloading {url}"));
    }
    let actual_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if let Err(e) = verify_sha256(&actual_hex, expected_sha256) {
        remove_temp_file(&part);
        warn!(
            target: TRACE_TARGET,
            op = "download",
            url,
            dest = %dest.display(),
            bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %e,
            "download failed: sha256 mismatch"
        );
        return Err(e).with_context(|| format!("downloading {url}"));
    }
    std::fs::rename(&part, dest)
        .with_context(|| format!("renaming {} -> {}", part.display(), dest.display()))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    info!(
        target: TRACE_TARGET,
        op = "download",
        url,
        dest = %dest.display(),
        bytes,
        elapsed_ms,
        "done"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn model_cache_path_accepts_plain_filenames_only() {
        let root = Path::new("/models");
        assert_eq!(
            model_cache_path(root, "model.gguf").unwrap(),
            PathBuf::from("/models/model.gguf")
        );
        assert!(model_cache_path(root, "../outside.gguf").is_err());
        assert!(model_cache_path(root, "nested/model.gguf").is_err());
        assert!(model_cache_path(root, "/tmp/model.gguf").is_err());
        assert!(model_cache_path(root, r"nested\model.gguf").is_err());
        assert!(model_cache_path(root, "").is_err());
    }

    #[test]
    fn verify_download_len_accepts_exact_match() {
        assert!(verify_download_len(2_700_000_000, Some(2_700_000_000)).is_ok());
    }

    #[test]
    fn verify_download_len_accepts_when_length_unknown() {
        assert!(verify_download_len(123, None).is_ok());
    }

    #[test]
    fn verify_download_len_rejects_truncated_download() {
        let err = verify_download_len(40, Some(100)).unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "got: {err}");
        assert!(err.contains("40"), "got: {err}");
        assert!(err.contains("100"), "got: {err}");
    }

    #[test]
    fn verify_download_len_rejects_overlong_download() {
        assert!(verify_download_len(120, Some(100)).is_err());
    }

    fn test_file(filename: &str, url: &str) -> ModelFile {
        ModelFile {
            role: crate::types::ModelFileRole::Model,
            url: url.to_string(),
            filename: filename.to_string(),
            approx_bytes: None,
            sha256: None,
        }
    }

    #[test]
    fn ensure_file_returns_cached_path_without_network() {
        // A file already present must be returned as-is — `ensure_file`
        // never touches the network, so an unreachable URL is fine.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("cached.gguf"), b"already here").unwrap();
        let path = ensure_file(
            dir.path(),
            &test_file("cached.gguf", "https://example.invalid/x"),
        )
        .unwrap();
        assert_eq!(path, dir.path().join("cached.gguf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"already here");
    }

    #[test]
    fn ensure_file_rejects_path_traversal_before_any_network() {
        let dir = tempdir().unwrap();
        let err = ensure_file(
            dir.path(),
            &test_file("../escape.gguf", "https://example.invalid/x"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("plain file name"), "got: {err}");
    }

    // -----------------------------------------------------------------
    // verify_sha256 — the integrity gate for registry-pinned hashes.
    // -----------------------------------------------------------------

    #[test]
    fn verify_sha256_accepts_match_and_absence() {
        assert!(verify_sha256("abc123", Some("abc123")).is_ok());
        assert!(
            verify_sha256("abc123", Some("ABC123")).is_ok(),
            "case-insensitive"
        );
        assert!(
            verify_sha256("abc123", Some(" abc123 ")).is_ok(),
            "whitespace-tolerant"
        );
        assert!(
            verify_sha256("abc123", None).is_ok(),
            "legacy rows have no hash"
        );
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let err = verify_sha256("abc123", Some("def456"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "got: {err}");
        assert!(
            err.contains("abc123") && err.contains("def456"),
            "must name both digests: {err}"
        );
    }

    // -----------------------------------------------------------------
    // remove_temp_file + TempFileGuard — the shared best-effort cleanup
    // primitives every engine routes its per-job scratch files through.
    // Owned here (the shared engine-provisioning module) so the sdcpp
    // output guard and the onnx init/mask cleanup share one tested
    // implementation instead of each rolling its own silent removal.
    // -----------------------------------------------------------------

    #[test]
    fn remove_temp_file_deletes_an_existing_file_quietly() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("artefact.webp");
        std::fs::write(&f, b"bytes").unwrap();
        let out = crate::test_support::capture({
            let f = f.clone();
            move || remove_temp_file(&f)
        });
        assert!(!f.exists(), "file should be gone after cleanup");
        assert!(
            !out.contains("failed to remove temp file"),
            "the success path must not warn: {out:?}"
        );
    }

    #[test]
    fn remove_temp_file_ignores_a_missing_file() {
        let dir = tempdir().unwrap();
        let out = crate::test_support::capture({
            let missing = dir.path().join("never.part");
            move || remove_temp_file(&missing)
        });
        assert!(
            !out.contains("failed to remove temp file"),
            "a not-found temp file is the desired end state: {out:?}"
        );
    }

    #[test]
    fn remove_temp_file_surfaces_a_failed_removal() {
        // Pointing the helper at a directory makes `remove_file` fail on
        // every platform (it refuses to unlink a dir): the closest
        // portable stand-in for a locked / permission-denied temp file.
        let dir = tempdir().unwrap();
        let stubborn = dir.path().join("subdir");
        std::fs::create_dir(&stubborn).unwrap();
        let out = crate::test_support::capture(move || remove_temp_file(&stubborn));
        assert!(
            out.contains("failed to remove temp file"),
            "a failed removal must surface in the logs: {out:?}"
        );
        assert!(
            out.contains("subdir"),
            "the warning must name the offending path: {out:?}"
        );
        assert!(
            out.contains("cleanup"),
            "the warning should tag the cleanup op: {out:?}"
        );
    }

    #[test]
    fn temp_file_guard_removes_every_registered_file_on_drop() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.webp");
        let init = dir.path().join("out-init.png");
        std::fs::write(&out, b"image").unwrap();
        std::fs::write(&init, b"init").unwrap();
        {
            let mut guard = TempFileGuard::new();
            guard.push(out.clone());
            guard.push(init.clone());
            assert!(out.exists() && init.exists(), "files present before drop");
        }
        assert!(!out.exists(), "output temp must be removed on drop");
        assert!(!init.exists(), "init-image temp must be removed on drop");
    }

    #[test]
    fn temp_file_guard_tolerates_a_file_that_never_materialised() {
        // A path registered before its download runs (so an early
        // failure drops a guard pointing at a file that never existed)
        // is the desired end state, not a cleanup warning.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-written.webp");
        let out = crate::test_support::capture(move || {
            let mut guard = TempFileGuard::new();
            guard.push(missing);
            drop(guard);
        });
        assert!(
            !out.contains("failed to remove temp file"),
            "a never-created temp file must not warn on cleanup: {out:?}"
        );
    }
}
