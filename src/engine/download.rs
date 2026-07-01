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

/// Pure core of the disk-space preflight: given the free bytes on the
/// cache filesystem and a file's declared size, refuse the download
/// when it cannot fit (with 10% headroom for the `.part` → rename
/// dance and concurrent growth).  Failing here — with both numbers in
/// the message — beats streaming gigabytes into ENOSPC and surfacing
/// an inscrutable io error mid-body.
pub fn check_disk_space(available: u64, approx_bytes: u64, filename: &str) -> Result<()> {
    let required = approx_bytes.saturating_add(approx_bytes / 10);
    if available < required {
        bail!(
            "not enough disk space for {filename}: need ~{required} bytes \
             (declared {approx_bytes} + 10% headroom) but only {available} \
             bytes are free on the models filesystem — free up space or \
             move models_root"
        );
    }
    Ok(())
}

/// IO half of the preflight: probe the free space under `dir` and run
/// [`check_disk_space`].  A file with no (or zero) declared size, or a
/// failed probe (exotic filesystems), skips the check — the preflight
/// is an early-warning gate, not a correctness gate; the length +
/// sha256 verification after the stream stays authoritative.
pub fn preflight_disk_space(dir: &Path, filename: &str, approx_bytes: Option<u64>) -> Result<()> {
    let Some(needed) = approx_bytes.filter(|b| *b > 0) else {
        return Ok(());
    };
    match fs4::available_space(dir) {
        Ok(available) => check_disk_space(available, needed, filename),
        Err(e) => {
            warn!(
                target: TRACE_TARGET,
                op = "preflight",
                dir = %dir.display(),
                filename,
                error = %e,
                "free-space probe failed; skipping the disk preflight"
            );
            Ok(())
        }
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

/// Sniff an image's container format from its leading magic bytes and
/// return the file extension `sd-cli` expects for it, or `None` when
/// the bytes match no format we hand to `sd-cli`.
///
/// `sd-cli`'s `media_io` loader picks its decoder purely from the file
/// **extension**, not the content — so a JPEG saved as `foo.webp`, or a
/// webp saved as `foo.png`, fails with `load image from '...' failed`.
/// The studio serves asset URLs like `latest.webp` whose bytes are
/// often actually JPEG, so the worker must name the on-disk tempfile
/// after the real content for the decoder to pick correctly.
pub fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.len() >= sig.len() && &bytes[..sig.len()] == sig;
    if starts(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("png")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("gif")
    } else if starts(b"BM") {
        Some("bmp")
    } else if starts(&[0x49, 0x49, 0x2a, 0x00]) || starts(&[0x4d, 0x4d, 0x00, 0x2a]) {
        Some("tif")
    } else {
        None
    }
}

/// Make a downloaded input image (init / mask / reference) safe to hand
/// to `sd-cli` by naming it after its **actual** content format.
///
/// The worker first names the tempfile from the URL's extension, but
/// studio asset URLs lie (`latest.webp` is frequently JPEG bytes).
/// `sd-cli` selects its image decoder from the file extension, so a
/// mismatched name makes every img2img / edit / inpaint job fail with
/// `load image from '...' failed`.  Here we sniff the real format from
/// the file's magic bytes and, when it disagrees with the current
/// extension, rename the file to a sibling with the correct one,
/// returning the path the engine should consume.  Unknown or
/// already-correct content passes straight through.
///
/// The caller owns cleanup: when the returned path differs from the
/// input it is the same bytes under a new name, so it (not the
/// original) must be registered with the job's [`TempFileGuard`].
pub fn ensure_correct_image_extension(path: &Path) -> Result<PathBuf> {
    let mut header = [0u8; 16];
    let read = {
        use std::io::Read;
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening input image {}", path.display()))?;
        file.read(&mut header)
            .with_context(|| format!("reading input image header {}", path.display()))?
    };
    let Some(actual_ext) = sniff_image_extension(&header[..read]) else {
        return Ok(path.to_path_buf());
    };
    let current_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    // `jpeg` and `jpg` are the same decoder to sd-cli — don't churn the
    // file when only the spelling differs.
    let matches = current_ext.as_deref() == Some(actual_ext)
        || (actual_ext == "jpg" && current_ext.as_deref() == Some("jpeg"));
    if matches {
        return Ok(path.to_path_buf());
    }
    let corrected = path.with_extension(actual_ext);
    std::fs::rename(path, &corrected)
        .with_context(|| format!("renaming {} -> {}", path.display(), corrected.display()))?;
    info!(
        target: TRACE_TARGET,
        op = "sniff",
        from = %path.display(),
        to = %corrected.display(),
        actual_ext,
        "renamed input image to match its actual format for sd-cli"
    );
    Ok(corrected)
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
    preflight_disk_space(dir, filename, file.approx_bytes)?;
    download_file_verified(url, &local, file.sha256.as_deref())
        .with_context(|| format!("downloading {filename} ({url}) -> {}", local.display()))?;
    Ok(local)
}

/// Parse the start offset out of a `Content-Range: bytes <start>-<end>/<total>`
/// header.  Returns `None` for anything that doesn't match that shape
/// (the caller then falls back to a fresh full download).
pub fn content_range_start(header: &str) -> Option<u64> {
    header
        .trim()
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .parse()
        .ok()
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
///
/// A leftover `<dest>.part` from an interrupted run is **resumed** via
/// an HTTP `Range` request instead of re-fetching multi-GiB models
/// from byte zero: the existing prefix is hashed, the remainder is
/// appended, and the final sha256 covers the assembled whole.  Servers
/// that ignore the range (200) fall back to a fresh full download;
/// `416` or a `Content-Range` that doesn't start where we asked drops
/// the stale part and restarts clean.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn download_file_verified(url: &str, dest: &Path, expected_sha256: Option<&str>) -> Result<()> {
    // Transport gate first: a plaintext-http model URL is a MITM away
    // from model poisoning, so it never gets a request at all.
    crate::net::validate_download_url(url, "model file")?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = dest.with_extension("part");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resume_from = std::fs::metadata(&part)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0);
    info!(
        target: TRACE_TARGET,
        op = "download",
        url,
        dest = %dest.display(),
        resume_from,
        "starting"
    );
    let started = Instant::now();
    let mut request = client.get(url);
    if resume_from > 0 {
        request = request.header("range", format!("bytes={resume_from}-"));
    }
    let mut response = match request.send() {
        Ok(response) => response,
        Err(e) => {
            // A connection-level failure (DNS, TLS, timeout, or a
            // connection closed before the declared body completed)
            // must leave the same terminal breadcrumb as the other
            // failure modes below — otherwise an operator filtering
            // this target sees the "starting" line then silence.
            warn!(
                target: TRACE_TARGET,
                op = "download",
                url,
                dest = %dest.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "download failed: request error"
            );
            return Err(e).context("GET");
        }
    };
    let status = response.status();
    if resume_from > 0 && status.as_u16() == 416 {
        // The server can't satisfy the range (stale / already-complete
        // part, or the remote file changed) — drop it and start clean.
        info!(
            target: TRACE_TARGET,
            op = "download",
            url,
            dest = %dest.display(),
            resume_from,
            "range not satisfiable; restarting the download from scratch"
        );
        remove_temp_file(&part);
        return download_file_verified(url, dest, expected_sha256);
    }
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
    let resuming = resume_from > 0 && status.as_u16() == 206;
    if resuming {
        // A compliant 206 answers exactly the range we asked for; a
        // Content-Range starting anywhere else would silently corrupt
        // the assembled file, so verify before appending a byte.
        let range_start = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_start);
        if range_start != Some(resume_from) {
            warn!(
                target: TRACE_TARGET,
                op = "download",
                url,
                dest = %dest.display(),
                resume_from,
                content_range_start = range_start,
                "206 Content-Range does not start at our offset; restarting from scratch"
            );
            remove_temp_file(&part);
            return download_file_verified(url, dest, expected_sha256);
        }
    }
    // For a 206 this is the *remainder* length — exactly what we are
    // about to stream, so the post-stream length check stays valid.
    let expected_len = response.content_length();
    let mut hasher = Sha256::new();
    let file = if resuming {
        // Fold the existing prefix into the digest so the final hash
        // covers the assembled whole, then append.
        let mut existing = std::fs::File::open(&part)
            .with_context(|| format!("opening partial download {}", part.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            use std::io::Read as _;
            let read = existing
                .read(&mut buf)
                .with_context(|| format!("hashing partial download {}", part.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .with_context(|| format!("reopening {} for append", part.display()))?
    } else {
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?
    };
    let mut writer = HashingWriter {
        inner: file,
        hasher,
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
        resumed_from = if resuming { resume_from } else { 0 },
        elapsed_ms,
        "done"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // -----------------------------------------------------------------
    // sniff_image_extension / ensure_correct_image_extension — the guard
    // that names a downloaded base after its real content so sd-cli's
    // extension-keyed `media_io` decoder picks the right codec.  Studio
    // asset URLs lie (`latest.webp` is often JPEG bytes); a mismatched
    // name was failing every img2img / edit / inpaint job with
    // `load image from '...' failed`.
    // -----------------------------------------------------------------

    /// A tiny lossy-VP8 webp (one of the formats studio bases arrive
    /// in) used to exercise the webp signature branch.
    const LOSSY_WEBP: &[u8] = include_bytes!("../../tests/fixtures/lossy-vp8.webp");

    #[test]
    fn sniff_image_extension_maps_each_magic_to_an_sd_cli_extension() {
        assert_eq!(sniff_image_extension(LOSSY_WEBP), Some("webp"));
        assert_eq!(
            sniff_image_extension(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]),
            Some("jpg"),
            "JPEG (the bytes studio serves under .webp URLs)"
        );
        assert_eq!(
            sniff_image_extension(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("png")
        );
        assert_eq!(sniff_image_extension(b"GIF89a..."), Some("gif"));
        assert_eq!(sniff_image_extension(b"BM......"), Some("bmp"));
        assert_eq!(
            sniff_image_extension(&[0x49, 0x49, 0x2a, 0x00]),
            Some("tif")
        );
        // A RIFF container that is not WEBP (e.g. a WAV) is not an image.
        assert_eq!(sniff_image_extension(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        // Unknown / too-short content yields no opinion.
        assert_eq!(sniff_image_extension(b"\x00\x01\x02"), None);
        assert_eq!(sniff_image_extension(b""), None);
    }

    #[test]
    fn ensure_correct_image_extension_renames_jpeg_served_as_webp() {
        // The exact prod failure: bytes are JPEG but the file is named
        // `.webp` (from the lying URL).  It must be renamed to `.jpg`.
        let dir = tempdir().unwrap();
        let mislabelled = dir.path().join("out-init.webp");
        std::fs::write(
            &mislabelled,
            [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46],
        )
        .unwrap();

        let corrected = ensure_correct_image_extension(&mislabelled).unwrap();

        assert_eq!(corrected, dir.path().join("out-init.jpg"));
        assert!(corrected.exists(), "renamed file carries the bytes");
        assert!(
            !mislabelled.exists(),
            "the misnamed file is gone after rename"
        );
    }

    #[test]
    fn ensure_correct_image_extension_renames_webp_served_as_png() {
        let dir = tempdir().unwrap();
        let mislabelled = dir.path().join("out-init.png");
        std::fs::write(&mislabelled, LOSSY_WEBP).unwrap();

        let corrected = ensure_correct_image_extension(&mislabelled).unwrap();

        assert_eq!(corrected, dir.path().join("out-init.webp"));
        assert!(corrected.exists() && !mislabelled.exists());
    }

    #[test]
    fn ensure_correct_image_extension_leaves_correct_or_unknown_files_in_place() {
        let dir = tempdir().unwrap();
        // Already-correct png: returned verbatim, not renamed.
        let png = dir.path().join("out-mask.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
        assert_eq!(ensure_correct_image_extension(&png).unwrap(), png);
        assert!(png.exists());

        // `.jpeg` spelling for JPEG content is not churned to `.jpg`.
        let jpeg = dir.path().join("out-ref.jpeg");
        std::fs::write(&jpeg, [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]).unwrap();
        assert_eq!(ensure_correct_image_extension(&jpeg).unwrap(), jpeg);
        assert!(jpeg.exists() && !dir.path().join("out-ref.jpg").exists());

        // Unknown content (no recognised magic) passes through untouched.
        let unknown = dir.path().join("out-init.webp");
        std::fs::write(&unknown, [0x00, 0x01, 0x02, 0x03]).unwrap();
        assert_eq!(ensure_correct_image_extension(&unknown).unwrap(), unknown);
        assert!(unknown.exists());
    }

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

    // -----------------------------------------------------------------
    // Disk-space preflight — refuses a download that cannot fit before
    // any bytes stream, instead of dying on ENOSPC mid-body.
    // -----------------------------------------------------------------

    #[test]
    fn check_disk_space_accepts_a_fit_with_headroom() {
        // 100 declared + 10% headroom = 110 required.
        assert!(check_disk_space(110, 100, "m.gguf").is_ok());
        assert!(check_disk_space(1_000, 100, "m.gguf").is_ok());
    }

    #[test]
    fn check_disk_space_rejects_when_it_cannot_fit() {
        let err = check_disk_space(109, 100, "m.gguf")
            .unwrap_err()
            .to_string();
        assert!(err.contains("m.gguf"), "must name the file: {err}");
        assert!(err.contains("109"), "must name the available bytes: {err}");
        assert!(err.contains("100"), "must name the declared size: {err}");
        assert!(
            err.contains("models_root"),
            "must tell the operator what to change: {err}"
        );
    }

    #[test]
    fn check_disk_space_survives_huge_declared_sizes() {
        // The +10% headroom saturates instead of overflowing near
        // u64::MAX — an overflow would wrap `required` to a tiny number
        // and wave an impossible download through.
        assert!(check_disk_space(u64::MAX - 1, u64::MAX, "m.gguf").is_err());
        assert!(check_disk_space(u64::MAX, u64::MAX, "m.gguf").is_ok());
    }

    #[test]
    fn preflight_skips_unknown_or_zero_sizes_and_checks_known_ones() {
        let dir = tempdir().unwrap();
        // Unknown / zero sizes: nothing to check.
        preflight_disk_space(dir.path(), "m.gguf", None).unwrap();
        preflight_disk_space(dir.path(), "m.gguf", Some(0)).unwrap();
        // A tiny known size passes on any real filesystem.
        preflight_disk_space(dir.path(), "m.gguf", Some(1024)).unwrap();
        // An absurd size fails against real free space.
        assert!(preflight_disk_space(dir.path(), "m.gguf", Some(u64::MAX / 2)).is_err());
    }

    // -----------------------------------------------------------------
    // Content-Range parsing — the resume-safety check that stops a
    // server answering the wrong range from corrupting the assembly.
    // -----------------------------------------------------------------

    #[test]
    fn content_range_start_parses_the_standard_shape() {
        assert_eq!(content_range_start("bytes 10-19/20"), Some(10));
        assert_eq!(content_range_start(" bytes 0-99/1000 "), Some(0));
        assert_eq!(content_range_start("bytes 5-9/*"), Some(5));
    }

    #[test]
    fn content_range_start_rejects_other_shapes() {
        assert_eq!(content_range_start("bytes */20"), None);
        assert_eq!(content_range_start("items 10-19/20"), None);
        assert_eq!(content_range_start("garbage"), None);
        assert_eq!(content_range_start(""), None);
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
    // HashingWriter — streams the body into the cache file while
    // computing the sha256 that `verify_sha256` later checks.  The
    // integrity guarantee hinges on hashing *exactly* the bytes the
    // inner writer accepted: `write` slices `&buf[..written]`, so a
    // short write (inner takes only a prefix) must hash only that
    // prefix — the unwritten tail is re-offered by `io::copy` on the
    // next call.  Hashing the whole `buf` on a short write would
    // silently corrupt every digest and turn the integrity gate into a
    // false-reject.  The download integration test wraps a real `File`,
    // which never short-writes, so this prefix branch is only reachable
    // here.
    // -----------------------------------------------------------------

    /// A writer that accepts at most `max_per_write` bytes per call (to
    /// model a short write) and counts `flush` calls.
    struct ProbeWriter {
        sink: Vec<u8>,
        max_per_write: usize,
        flushes: usize,
    }

    impl Write for ProbeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let take = buf.len().min(self.max_per_write);
            self.sink.extend_from_slice(&buf[..take]);
            Ok(take)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hashing_writer_hashes_only_the_bytes_the_inner_accepted() {
        // The inner writer takes only 3 of the 8 offered bytes, so the
        // hasher must absorb just "abc" — proving the `&buf[..written]`
        // slice.  If `write` hashed the whole `buf`, the digest would be
        // sha256("abcdefgh") and this assertion would fail.
        let mut writer = HashingWriter {
            inner: ProbeWriter {
                sink: Vec::new(),
                max_per_write: 3,
                flushes: 0,
            },
            hasher: Sha256::new(),
        };
        let written = writer.write(b"abcdefgh").unwrap();
        assert_eq!(written, 3, "inner accepts at most 3 bytes per write");
        assert_eq!(writer.inner.sink, b"abc", "only the prefix reaches inner");
        assert_eq!(
            hex(&writer.hasher.finalize()),
            hex(&Sha256::digest(b"abc")),
            "hash covers only the accepted prefix"
        );
    }

    #[test]
    fn hashing_writer_digest_matches_a_short_writing_stream_end_to_end() {
        // Drive the writer the way `download_file_verified` does — via
        // `io::copy`, which re-offers the unwritten tail — through an
        // inner that only takes 4 bytes at a time.  The streamed bytes
        // and the final digest must both equal the full source, with no
        // double-hashing across the re-offered chunks.
        let source = b"the quick brown model weights".to_vec();
        let mut reader = source.as_slice();
        let mut writer = HashingWriter {
            inner: ProbeWriter {
                sink: Vec::new(),
                max_per_write: 4,
                flushes: 0,
            },
            hasher: Sha256::new(),
        };
        let copied = std::io::copy(&mut reader, &mut writer).unwrap();
        assert_eq!(copied as usize, source.len());
        assert_eq!(
            writer.inner.sink, source,
            "every byte reaches the cache file"
        );
        assert_eq!(
            hex(&writer.hasher.finalize()),
            hex(&Sha256::digest(&source)),
            "digest matches the full body"
        );
    }

    #[test]
    fn hashing_writer_flush_delegates_to_the_inner_writer() {
        let mut writer = HashingWriter {
            inner: ProbeWriter {
                sink: Vec::new(),
                max_per_write: usize::MAX,
                flushes: 0,
            },
            hasher: Sha256::new(),
        };
        writer.flush().unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.inner.flushes, 2, "flush is forwarded to inner");
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
