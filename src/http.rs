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

    pub fn register(
        &self,
        bootstrap_token: &str,
        cap: WorkerCapabilities,
        worker_id: Option<String>,
    ) -> Result<RegisterResponse> {
        let body = RegisterRequest {
            bootstrap_token: bootstrap_token.to_string(),
            capabilities: cap,
            worker_id,
        };
        let url = self.url("/workers/register");
        let started = Instant::now();
        let response = self
            .client
            .post(&url)
            .bearer_auth(bootstrap_token)
            .json(&body)
            .send()?;
        let response = self.check("register", &url, started, response)?;
        Ok(response.json()?)
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
        let mime = match ext {
            "png" => "image/png",
            "webp" => "image/webp",
            "wav" => "audio/wav",
            "mp3" => "audio/mpeg",
            "mp4" => "video/mp4",
            _ => "application/octet-stream",
        };
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
