//! Thin reqwest wrapper around the studio API.
//!
//! Every call goes through [`ApiClient::check`], which:
//!
//! - emits a structured `tracing` event on success (`debug`) and
//!   failure (`warn`) so operators can see what the worker is talking
//!   to without having to enable wire-level logging in reqwest, and
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

pub struct ApiClient {
    pub base_url: String,
    pub client: Client,
}

impl ApiClient {
    pub fn new(base_url: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("building reqwest client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
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
        Err(anyhow!("{op} failed: {status} — {body}"))
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
