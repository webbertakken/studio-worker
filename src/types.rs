//! Shared wire-format mirrors of the studio API.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    #[serde(rename = "machineName")]
    pub machine_name: String,
    pub username: String,
    #[serde(rename = "agentVersion")]
    pub agent_version: String,
    pub engine: String,
    #[serde(rename = "vramTotalGb")]
    pub vram_total_gb: f32,
    #[serde(rename = "vramThresholdGb")]
    pub vram_threshold_gb: f32,
    #[serde(rename = "autoEnabled")]
    pub auto_enabled: bool,
    #[serde(rename = "autoStart")]
    pub auto_start: bool,
    #[serde(rename = "supportedModels")]
    pub supported_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterRequest {
    #[serde(rename = "bootstrapToken")]
    pub bootstrap_token: String,
    pub capabilities: WorkerCapabilities,
    #[serde(rename = "workerId", skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterResponse {
    #[serde(rename = "workerId")]
    pub worker_id: String,
    #[serde(rename = "authToken")]
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatRequest {
    pub capabilities: WorkerCapabilities,
    #[serde(rename = "currentJobId", skip_serializing_if = "Option::is_none")]
    pub current_job_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobClaim {
    #[serde(rename = "jobId")]
    pub job_id: String,
    #[serde(rename = "gameId")]
    #[allow(dead_code)] // returned for diagnostics, not used directly in the loop yet
    pub game_id: String,
    #[serde(rename = "assetName")]
    #[allow(dead_code)] // returned for diagnostics, used by the API for upload
    pub asset_name: String,
    pub model: String,
    #[serde(rename = "vramGbEstimate")]
    pub vram_gb_estimate: f32,
    pub prompt: String,
    pub ext: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailRequest {
    pub error: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: String,
    pub category: String,
    pub message: String,
    #[serde(rename = "jobId", skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogBatch {
    pub entries: Vec<LogEntry>,
}
