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

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;
use tracing::{info, warn};

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

/// Best-effort removal of a partial `.part` download.  A `NotFound` is
/// the desired end state (something already cleaned it up); any other
/// failure is surfaced so a stuck temp file can't silently fill the
/// worker's disk over a long session.
pub fn remove_partial(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                target: TRACE_TARGET,
                op = "cleanup",
                path = %path.display(),
                error = %e,
                "failed to remove partial download"
            );
        }
    }
}

/// Ensure `filename` is present under `dir`, downloading it from `url`
/// when missing.  Returns the resolved local path.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn ensure_file(dir: &Path, filename: &str, url: &str) -> Result<PathBuf> {
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
    download_file(url, &local)
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
    if !response.status().is_success() {
        bail!("GET {url} -> {}", response.status());
    }
    let expected_len = response.content_length();
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let copied = std::io::copy(&mut response, &mut file);
    // Close the handle before any remove / rename so cleanup works on
    // Windows, where an open file can't be unlinked.
    drop(file);
    let bytes = match copied {
        Ok(bytes) => bytes,
        Err(e) => {
            remove_partial(&part);
            return Err(e).context("streaming body");
        }
    };
    if let Err(e) = verify_download_len(bytes, expected_len) {
        remove_partial(&part);
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

    #[test]
    fn ensure_file_returns_cached_path_without_network() {
        // A file already present must be returned as-is — `ensure_file`
        // never touches the network, so an unreachable URL is fine.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("cached.gguf"), b"already here").unwrap();
        let path = ensure_file(dir.path(), "cached.gguf", "https://example.invalid/x").unwrap();
        assert_eq!(path, dir.path().join("cached.gguf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"already here");
    }

    #[test]
    fn ensure_file_rejects_path_traversal_before_any_network() {
        let dir = tempdir().unwrap();
        let err = ensure_file(dir.path(), "../escape.gguf", "https://example.invalid/x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("plain file name"), "got: {err}");
    }

    #[test]
    fn remove_partial_ignores_a_missing_file() {
        let dir = tempdir().unwrap();
        let out = crate::test_support::capture({
            let missing = dir.path().join("never.part");
            move || remove_partial(&missing)
        });
        assert!(
            !out.contains("failed to remove partial download"),
            "a not-found partial is the desired end state: {out:?}"
        );
    }

    #[test]
    fn remove_partial_surfaces_a_failed_removal() {
        // Pointing the helper at a directory makes `remove_file` fail on
        // every platform (it refuses to unlink a dir).
        let dir = tempdir().unwrap();
        let stubborn = dir.path().join("subdir");
        std::fs::create_dir(&stubborn).unwrap();
        let out = crate::test_support::capture(move || remove_partial(&stubborn));
        assert!(
            out.contains("failed to remove partial download"),
            "a failed removal must surface in the logs: {out:?}"
        );
    }
}
