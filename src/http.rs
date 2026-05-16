//! Thin reqwest wrapper around the studio API.
use crate::types::*;
use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use std::time::Duration;

/// Base path under which the worker endpoints are mounted.
const API_PREFIX: &str = "/graphics/api";

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
        let response = self
            .client
            .post(self.url("/workers/register"))
            .bearer_auth(bootstrap_token)
            .json(&body)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "register failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(response.json()?)
    }

    pub fn heartbeat(
        &self,
        worker_id: &str,
        token: &str,
        cap: WorkerCapabilities,
        current_job_id: Option<String>,
    ) -> Result<()> {
        let body = HeartbeatRequest {
            capabilities: cap,
            current_job_id,
        };
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/heartbeat")))
            .bearer_auth(token)
            .json(&body)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "heartbeat failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// Returns `Ok(None)` on HTTP 204 (no jobs).
    pub fn claim(&self, worker_id: &str, token: &str) -> Result<Option<JobClaim>> {
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/claim")))
            .bearer_auth(token)
            .send()?;
        if response.status().as_u16() == 204 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(anyhow!(
                "claim failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(Some(response.json()?))
    }

    /// Complete a job with binary output (image / audio / video).
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
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/jobs/{job_id}/complete")))
            .bearer_auth(token)
            .multipart(form)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "complete failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// Complete a job with structured JSON output (LLM / STT).
    pub fn complete_json(
        &self,
        worker_id: &str,
        token: &str,
        job_id: &str,
        prompt: &str,
        result: &serde_json::Value,
    ) -> Result<()> {
        let body = serde_json::json!({
            "jobId": job_id,
            "prompt": prompt,
            "result": result,
            "resultKind": "json",
        });
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/jobs/{job_id}/complete-json")))
            .bearer_auth(token)
            .json(&body)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "complete-json failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(())
    }

    pub fn fail(
        &self,
        worker_id: &str,
        token: &str,
        job_id: &str,
        error: &str,
        retryable: bool,
    ) -> Result<()> {
        let body = FailRequest {
            error: error.to_string(),
            retryable,
        };
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/jobs/{job_id}/fail")))
            .bearer_auth(token)
            .json(&body)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "fail failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(())
    }

    pub fn ship_logs(&self, worker_id: &str, token: &str, batch: LogBatch) -> Result<()> {
        let response = self
            .client
            .post(self.url(&format!("/workers/{worker_id}/logs")))
            .bearer_auth(token)
            .json(&batch)
            .send()?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "log ship failed: {} — {}",
                response.status(),
                response.text().unwrap_or_default()
            ));
        }
        Ok(())
    }
}
