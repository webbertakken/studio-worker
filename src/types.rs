//! Shared wire-format mirrors of the studio API.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Task kinds — every job claimed by the worker is one of these.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Image,
    Llm,
    AudioStt,
    AudioTts,
    Video,
}

impl TaskKind {
    pub const ALL: [TaskKind; 5] = [
        TaskKind::Image,
        TaskKind::Llm,
        TaskKind::AudioStt,
        TaskKind::AudioTts,
        TaskKind::Video,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Image => "image",
            TaskKind::Llm => "llm",
            TaskKind::AudioStt => "audio_stt",
            TaskKind::AudioTts => "audio_tts",
            TaskKind::Video => "video",
        }
    }
}

// ---------------------------------------------------------------------------
// Per-kind task parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageParams {
    pub prompt: String,
    #[serde(default = "default_image_dim")]
    pub width: u32,
    #[serde(default = "default_image_dim")]
    pub height: u32,
    #[serde(default = "default_steps")]
    pub steps: u32,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default = "default_image_ext")]
    pub ext: String,
}

fn default_image_dim() -> u32 {
    512
}
fn default_steps() -> u32 {
    20
}
fn default_image_ext() -> String {
    "webp".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmParams {
    pub messages: Vec<ChatMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f32 {
    0.7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSttParams {
    /// HTTPS URL to fetch the audio bytes from (e.g. R2 signed URL).
    pub input_url: String,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTtsParams {
    pub text: String,
    #[serde(default = "default_voice")]
    pub voice: String,
    #[serde(default = "default_audio_ext")]
    pub ext: String,
}

fn default_voice() -> String {
    "default".into()
}
fn default_audio_ext() -> String {
    "wav".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoParams {
    pub prompt: String,
    #[serde(default = "default_video_seconds")]
    pub seconds: f32,
    #[serde(default = "default_image_dim")]
    pub width: u32,
    #[serde(default = "default_image_dim")]
    pub height: u32,
    #[serde(default = "default_video_ext")]
    pub ext: String,
}

fn default_video_seconds() -> f32 {
    2.0
}
fn default_video_ext() -> String {
    "mp4".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Task {
    Image(ImageParams),
    Llm(LlmParams),
    AudioStt(AudioSttParams),
    AudioTts(AudioTtsParams),
    Video(VideoParams),
}

impl Task {
    pub fn kind(&self) -> TaskKind {
        match self {
            Task::Image(_) => TaskKind::Image,
            Task::Llm(_) => TaskKind::Llm,
            Task::AudioStt(_) => TaskKind::AudioStt,
            Task::AudioTts(_) => TaskKind::AudioTts,
            Task::Video(_) => TaskKind::Video,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-kind task results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum TaskResult {
    /// Binary image (webp/png/...) with the chosen extension.
    Image { bytes: Vec<u8>, ext: String },
    /// JSON response from an LLM call (free-form to mirror common APIs).
    Llm { json: serde_json::Value },
    /// JSON transcript.
    AudioStt { json: serde_json::Value },
    /// Binary audio (wav/mp3/...) with the chosen extension.
    AudioTts { bytes: Vec<u8>, ext: String },
    /// Binary video (mp4/webm/...) with the chosen extension.
    Video { bytes: Vec<u8>, ext: String },
}

impl TaskResult {
    pub fn kind(&self) -> TaskKind {
        match self {
            TaskResult::Image { .. } => TaskKind::Image,
            TaskResult::Llm { .. } => TaskKind::Llm,
            TaskResult::AudioStt { .. } => TaskKind::AudioStt,
            TaskResult::AudioTts { .. } => TaskKind::AudioTts,
            TaskResult::Video { .. } => TaskKind::Video,
        }
    }
}

// ---------------------------------------------------------------------------
// Worker capabilities + registration
// ---------------------------------------------------------------------------

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
    /// Flat list of models, kept for backward compat with the existing studio
    /// API that doesn't know about kinds yet.  Equivalent to
    /// `supported_models_per_kind[Image]`.
    #[serde(rename = "supportedModels")]
    pub supported_models: Vec<String>,
    /// New: task kinds this worker can serve.
    #[serde(rename = "taskKinds", default)]
    pub task_kinds: Vec<TaskKind>,
    /// New: per-kind supported model ids.
    #[serde(rename = "supportedModelsPerKind", default)]
    pub supported_models_per_kind: BTreeMap<TaskKind, Vec<String>>,
}

// ---------------------------------------------------------------------------
// Auto-register wire format — the only registration path.
//
// Worker POSTs `/workers/register-request` with hostname / username /
// VRAM / supported models, gets a `requestId` back, and polls
// `/workers/register-requests/:id` until the operator approves or
// rejects from the studio dashboard.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AutoRegisterRequest {
    /// Per-install UUID stable across worker restarts on the same
    /// machine.  The studio uses it to dedup re-submissions.
    #[serde(rename = "installId")]
    pub install_id: String,
    /// SHA-256 hex of the worker-side `registration_secret`.  The
    /// worker keeps the secret locally and presents it as a Bearer
    /// token when polling for status; only the hash leaves the box.
    #[serde(rename = "registrationSecretHash")]
    pub registration_secret_hash: String,
    /// Optional human label the operator sees in the Pending Workers
    /// panel (e.g. "alice's gaming rig").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Full capability snapshot — hostname, username, engine, VRAM,
    /// supported models so the operator can decide.
    pub capabilities: WorkerCapabilities,
    /// `studio-worker/<version>` so the operator sees stale clients.
    #[serde(rename = "userAgent")]
    pub user_agent: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AutoRegisterRequestResponse {
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Always `"pending"` on first response.  Idempotent dedup may
    /// return the existing requestId for the same
    /// `(installId, sourceIp)` tuple — status is still "pending".
    pub status: String,
}

/// Tagged union returned by `GET /workers/register-requests/:id`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum RegisterStatus {
    Pending,
    Approved {
        #[serde(rename = "workerId")]
        worker_id: String,
        #[serde(rename = "authToken")]
        auth_token: String,
    },
    Rejected {
        #[serde(default)]
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatRequest {
    pub capabilities: WorkerCapabilities,
    #[serde(rename = "currentJobId", skip_serializing_if = "Option::is_none")]
    pub current_job_id: Option<String>,
}

// ---------------------------------------------------------------------------
// JobClaim — backward-compatible with the existing image-only studio API.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct JobClaim {
    #[serde(rename = "jobId")]
    pub job_id: String,
    #[serde(rename = "gameId")]
    #[allow(dead_code)]
    pub game_id: String,
    #[serde(rename = "assetName")]
    pub asset_name: String,
    pub model: String,
    #[serde(rename = "vramGbEstimate")]
    pub vram_gb_estimate: f32,
    /// Legacy field (image prompt).  Ignored when `task` is present.
    #[serde(default)]
    pub prompt: String,
    /// Legacy field (image extension).  Ignored when `task` is present.
    #[serde(default = "default_image_ext")]
    pub ext: String,
    /// New: structured task spec.  When absent the worker reconstructs
    /// an `Task::Image` from `prompt` + `ext` above for backward compat.
    #[serde(default)]
    pub task: Option<Task>,
}

impl JobClaim {
    /// Resolve the structured task, applying the legacy fallback if
    /// `task` is missing.
    pub fn resolved_task(&self) -> Task {
        if let Some(t) = self.task.clone() {
            return t;
        }
        Task::Image(ImageParams {
            prompt: self.prompt.clone(),
            width: 512,
            height: 512,
            steps: 20,
            seed: None,
            ext: self.ext.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FailRequest {
    pub error: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ui", derive(PartialEq, Eq))]
pub struct LogEntry {
    pub ts: String,
    pub level: String,
    pub category: String,
    pub message: String,
    #[serde(rename = "jobId", default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBatch {
    pub entries: Vec<LogEntry>,
}

// ---------------------------------------------------------------------------
// Release feed (auto-update)
// ---------------------------------------------------------------------------

/// Subset of the GitHub Releases API we care about.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_claim_with_no_task_falls_back_to_image() {
        let json = serde_json::json!({
            "jobId": "j-1",
            "gameId": "g-1",
            "assetName": "g-1/creatures/x",
            "model": "synthetic",
            "vramGbEstimate": 1.0,
            "prompt": "a stone golem",
            "ext": "webp",
        });
        let claim: JobClaim = serde_json::from_value(json).unwrap();
        match claim.resolved_task() {
            Task::Image(p) => {
                assert_eq!(p.prompt, "a stone golem");
                assert_eq!(p.ext, "webp");
            }
            other => panic!("expected image, got {:?}", other),
        }
    }

    #[test]
    fn job_claim_with_explicit_llm_task() {
        let json = serde_json::json!({
            "jobId": "j-2",
            "gameId": "g-1",
            "assetName": "g-1/conversations/x",
            "model": "llama-3.1-8b",
            "vramGbEstimate": 8.0,
            "task": {
                "kind": "llm",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 32,
                "temperature": 0.5,
            },
        });
        let claim: JobClaim = serde_json::from_value(json).unwrap();
        match claim.resolved_task() {
            Task::Llm(p) => {
                assert_eq!(p.messages.len(), 1);
                assert_eq!(p.max_tokens, 32);
            }
            other => panic!("expected llm, got {:?}", other),
        }
    }

    #[test]
    fn job_claim_with_explicit_image_task() {
        let json = serde_json::json!({
            "jobId": "j-3",
            "gameId": "g-1",
            "assetName": "g-1/creatures/y",
            "model": "synthetic",
            "vramGbEstimate": 8.0,
            "task": {
                "kind": "image",
                "prompt": "a koi",
                "width": 1024,
                "height": 1024,
                "steps": 30,
                "ext": "png",
            },
        });
        let claim: JobClaim = serde_json::from_value(json).unwrap();
        match claim.resolved_task() {
            Task::Image(p) => {
                assert_eq!(p.prompt, "a koi");
                assert_eq!(p.width, 1024);
                assert_eq!(p.ext, "png");
            }
            other => panic!("expected image, got {:?}", other),
        }
    }

    #[test]
    fn task_kinds_round_trip_via_json() {
        for kind in TaskKind::ALL {
            let s = serde_json::to_string(&kind).unwrap();
            let back: TaskKind = serde_json::from_str(&s).unwrap();
            assert_eq!(kind, back);
        }
    }
}
