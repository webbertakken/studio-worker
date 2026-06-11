//! Thin reqwest wrapper around the studio API.
//!
//! Every call goes through [`ApiClient::check`], which:
//!
//! - emits a structured `tracing` event on success (`debug`) and
//!   failure (`warn`) so operators can see what the worker is talking
//!   to without having to enable wire-level logging in reqwest
//!   (`complete` also logs the upload byte size before the request so
//!   the attempted payload size is visible even when it never finishes),
//!   and
//! - turns non-2xx responses into an `anyhow` error tagged with the
//!   operation name so the existing log shipper messages stay legible.
use crate::types::*;
use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, Response};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Base path under which the worker endpoints are mounted.
const API_PREFIX: &str = "/graphics/api";

/// Tracing target used for every event emitted by the HTTP client.
/// Keeping it stable lets operators filter with
/// `RUST_LOG=studio_worker::http=debug` without touching the rest of
/// the agent's logs.
const TRACE_TARGET: &str = "studio_worker::http";

/// Typed non-2xx response error so callers can branch on the status
/// class (e.g. retry 5xx, never retry 4xx) without sniffing message
/// strings.  The rendered message keeps the legacy `<op> failed:
/// <status> — <body>` shape existing tests and log consumers expect.
#[derive(Debug, thiserror::Error)]
#[error("{op} failed: {status} — {body}")]
pub struct HttpStatusError {
    pub op: String,
    pub status: u16,
    pub body: String,
}

impl HttpStatusError {
    /// Server-side failures are worth retrying; 4xx contract errors
    /// are not.
    pub fn is_transient(&self) -> bool {
        self.status >= 500
    }
}

/// True when the upload failed for a reason a short retry can fix: a
/// 5xx from the studio or a transport-level error (connect refused,
/// timeout, broken pipe).  4xx contract errors return false.
pub fn is_transient_upload_error(e: &anyhow::Error) -> bool {
    if let Some(status) = e.downcast_ref::<HttpStatusError>() {
        return status.is_transient();
    }
    e.downcast_ref::<reqwest::Error>().is_some()
}

pub struct ApiClient {
    pub base_url: String,
    pub client: Client,
}

/// Process-wide blocking client, shared by every `ApiClient`.
/// `reqwest::blocking::Client` is an `Arc` around a connection pool;
/// rebuilding it per call (the old behaviour) re-did TLS setup and
/// threw the pool away between requests.
fn shared_client() -> Result<Client> {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let built = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building reqwest client")?;
    // A concurrent first call may have won the publish; either client
    // is fine — take whichever landed.
    Ok(CLIENT.get_or_init(|| built).clone())
}

impl ApiClient {
    pub fn new(base_url: String) -> Result<Self> {
        Ok(Self {
            base_url: normalize_base_url(&base_url)?,
            client: shared_client()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.base_url, API_PREFIX, path)
    }

    /// Inspect a response, log it, and convert non-2xx into an
    /// `anyhow` error.  `op` is the human-readable operation name used
    /// in the error message (kept stable for log-shipper consumers and
    /// existing tests).
    fn check(&self, op: &str, url: &str, started: Instant, response: Response) -> Result<Response> {
        let status = response.status();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if status.is_success() || status.as_u16() == 204 {
            debug!(
                target: TRACE_TARGET,
                op,
                endpoint = %url,
                status = status.as_u16(),
                elapsed_ms,
                "ok"
            );
            return Ok(response);
        }
        // Body read consumes the response; we only need it on the
        // failure path.
        let body = response.text().unwrap_or_default();
        warn!(
            target: TRACE_TARGET,
            op,
            endpoint = %url,
            status = status.as_u16(),
            elapsed_ms,
            body = %body,
            "{op} failed"
        );
        Err(HttpStatusError {
            op: op.to_string(),
            status: status.as_u16(),
            body,
        }
        .into())
    }

    // -----------------------------------------------------------------------
    // Auto-register (operator-approved) flow
    // -----------------------------------------------------------------------

    /// Create a Pending Workers row.  Unauthenticated on purpose —
    /// the studio rate-limits this endpoint by source IP and the
    /// operator manually approves before the worker can do anything.
    pub fn register_request(
        &self,
        payload: &AutoRegisterRequest,
    ) -> Result<AutoRegisterRequestResponse> {
        let url = self.url("/workers/register-request");
        let started = Instant::now();
        let response = self.client.post(&url).json(payload).send()?;
        let response = self.check("register-request", &url, started, response)?;
        Ok(response.json()?)
    }

    /// Poll the studio for the operator's decision on a previously
    /// submitted register-request.  Returns `Ok(None)` when the
    /// request id is unknown to the studio (likely cleaned up or
    /// never existed) so the orchestrator can drop the stale id and
    /// start a fresh one.  Auth is the raw `registration_secret`
    /// presented as a Bearer token.
    pub fn poll_register_status(
        &self,
        request_id: &str,
        registration_secret: &str,
    ) -> Result<Option<RegisterStatus>> {
        let url = self.url(&format!("/workers/register-requests/{request_id}"));
        let started = Instant::now();
        let response = self
            .client
            .get(&url)
            .bearer_auth(registration_secret)
            .send()?;
        if response.status().as_u16() == 404 {
            debug!(
                target: TRACE_TARGET,
                op = "register-poll",
                endpoint = %url,
                status = 404,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "register request not found (stale id; orchestrator will recreate)"
            );
            return Ok(None);
        }
        let response = self.check("register-poll", &url, started, response)?;
        Ok(Some(response.json()?))
    }

    /// Complete a job with binary output (image / audio / video).
    ///
    /// This is the only worker-side HTTP route that survives the WS
    /// migration: R2 multipart doesn't fit cleanly into WS frames.
    /// Heartbeats, claim/accept/reject, completeJson, fail, and log
    /// shipping all flow over the WS session owned by
    /// `ws::session::spawn_ws_session`.
    pub fn complete(
        &self,
        worker_id: &str,
        token: &str,
        job_id: &str,
        ext: &str,
        prompt: &str,
        image: Vec<u8>,
    ) -> Result<()> {
        let mime = mime_for_ext(ext);
        let bytes = image.len() as u64;
        // Emitted before the (potentially slow or failing) upload so the
        // attempted payload size is always in the operator's logs, even
        // when the request itself never completes.
        debug!(
            target: TRACE_TARGET,
            op = "complete",
            job_id,
            ext,
            mime,
            bytes,
            "uploading job result"
        );
        let part = reqwest::blocking::multipart::Part::bytes(image)
            .file_name(format!("{job_id}.{ext}"))
            .mime_str(mime)?;
        let form = reqwest::blocking::multipart::Form::new()
            .text("prompt", prompt.to_string())
            .text("ext", ext.to_string())
            .part("image", part);
        let url = self.url(&format!("/workers/{worker_id}/jobs/{job_id}/complete"));
        let started = Instant::now();
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()?;
        self.check("complete", &url, started, response)?;
        Ok(())
    }

    /// Like [`Self::complete`] but retries transient failures (5xx +
    /// transport errors) up to `retries` additional attempts, pausing
    /// `pause * attempt` between them.  4xx contract errors surface
    /// immediately.  Keeps a brief upload blip from costing a full GPU
    /// regeneration — a reported `Fail` makes the studio requeue and
    /// re-render the whole job.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_with_retry(
        &self,
        worker_id: &str,
        token: &str,
        job_id: &str,
        ext: &str,
        prompt: &str,
        image: Vec<u8>,
        retries: u32,
        pause: Duration,
    ) -> Result<()> {
        let mut attempt: u32 = 0;
        loop {
            match self.complete(worker_id, token, job_id, ext, prompt, image.clone()) {
                Ok(()) => return Ok(()),
                Err(e) if attempt < retries && is_transient_upload_error(&e) => {
                    attempt += 1;
                    warn!(
                        target: TRACE_TARGET,
                        op = "complete",
                        job_id,
                        attempt,
                        max_attempts = retries + 1,
                        error = %e,
                        "transient upload failure; retrying"
                    );
                    std::thread::sleep(pause * attempt);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn normalize_base_url(base_url: &str) -> Result<String> {
    let mut url =
        url::Url::parse(base_url).map_err(|e| anyhow!("invalid api_base_url {base_url:?}: {e}"))?;
    url.set_query(None);
    url.set_fragment(None);

    let trimmed_path = url.path().trim_end_matches('/').to_string();
    if trimmed_path.ends_with(API_PREFIX) {
        let without_prefix = trimmed_path[..trimmed_path.len() - API_PREFIX.len()].to_string();
        url.set_path(if without_prefix.is_empty() {
            "/"
        } else {
            &without_prefix
        });
    }

    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Map a binary output's file extension to the MIME type sent as the
/// multipart `complete` upload's `Content-Type`.  Single source of
/// truth: every engine that emits a `TaskResult` binary extension
/// (synthetic image → `png`/`webp`, sd-cpp → `webp`, tts → `wav`,
/// synthetic video → `webp`, the `video` feature → `gif`) routes
/// through here, so a new extension can't silently drift into
/// `application/octet-stream` and break the studio's stored
/// content-type.
pub fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_for_ext_maps_known_image_audio_video_types() {
        assert_eq!(mime_for_ext("png"), "image/png");
        assert_eq!(mime_for_ext("webp"), "image/webp");
        assert_eq!(mime_for_ext("gif"), "image/gif");
        assert_eq!(mime_for_ext("wav"), "audio/wav");
        assert_eq!(mime_for_ext("mp3"), "audio/mpeg");
        assert_eq!(mime_for_ext("mp4"), "video/mp4");
    }

    #[test]
    fn mime_for_ext_falls_back_to_octet_stream_for_unknown() {
        assert_eq!(mime_for_ext("bin"), "application/octet-stream");
        assert_eq!(mime_for_ext(""), "application/octet-stream");
    }

    #[test]
    fn normalize_base_url_strips_existing_graphics_api_prefix() {
        let api = ApiClient::new("https://studio.example/graphics/api/".into()).unwrap();
        assert_eq!(
            api.url("/workers/register-request"),
            "https://studio.example/graphics/api/workers/register-request"
        );
    }

    #[test]
    fn normalize_base_url_preserves_outer_mount_path() {
        let api = ApiClient::new("https://studio.example/custom/graphics/api".into()).unwrap();
        assert_eq!(
            api.url("/workers/register-request"),
            "https://studio.example/custom/graphics/api/workers/register-request"
        );
    }

    #[test]
    fn mime_for_ext_covers_every_extension_engines_emit() {
        // Lock the contract: each binary extension an engine actually
        // emits must resolve to a real MIME type, never the
        // octet-stream fallback.  `gif` is the one the `video`
        // feature produces and that regressed before this guard.
        for ext in ["png", "webp", "gif", "wav"] {
            assert_ne!(
                mime_for_ext(ext),
                "application/octet-stream",
                "engine output extension {ext:?} must map to a real MIME type"
            );
        }
    }
}
