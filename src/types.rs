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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageParams {
    pub prompt: String,
    /// Negative prompt — what the model should steer AWAY from.
    /// Optional: when `None`, `sd-cli` is invoked without
    /// `--negative-prompt`.  Wire-format key: `negativePrompt`.
    #[serde(default)]
    pub negative_prompt: Option<String>,
    /// HTTPS URL to a base image for image-to-image generation.
    /// When set, the worker downloads the bytes to a local tempfile
    /// and invokes `sd-cli --init-img <path>`.  Required for any
    /// task profile that declares the `initImage` capability.
    #[serde(default)]
    pub init_image_url: Option<String>,
    /// HTTPS URL to a black/white inpaint mask (white = the region the
    /// model may repaint). When set alongside `init_image_url`, the
    /// worker downloads it and invokes `sd-cli --mask <path>` so only
    /// the masked region changes. Wire-format key: `maskUrl`.
    #[serde(default)]
    pub mask_url: Option<String>,
    /// HTTPS URL to a reference image for instruction-edit models
    /// (e.g. Qwen-Image-Edit / Flux Kontext).  When set, the worker
    /// downloads it and invokes `sd-cli -r <path>` (reference mode)
    /// instead of the `--init-img`/`--strength`/`--mask` img2img path:
    /// the model regenerates the whole image from the reference per the
    /// instruction prompt; per-region clipping happens in the studio
    /// composite. Wire-format key: `refImageUrl`.
    #[serde(default)]
    pub ref_image_url: Option<String>,
    /// Denoise / noise-strength for i2i (0.0 = keep init image
    /// unchanged, 1.0 = full re-noise).  Maps to `sd-cli --strength`.
    #[serde(default)]
    pub denoise: Option<f32>,
    /// Classifier-free guidance scale.  Per-job override; falls back
    /// to `ModelSource.cliDefaults.cfgScale` when `None`.
    #[serde(default)]
    pub cfg_scale: Option<f32>,
    /// Sampler choice (`euler`, `euler_a`, `dpm++2m`, ...).  Per-job
    /// override; falls back to `ModelSource.cliDefaults.samplingMethod`.
    #[serde(default)]
    pub sampling_method: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmParams {
    pub messages: Vec<ChatMessage>,
    /// System prompt prepended to the conversation.  Engines that
    /// don't accept a separate system role inline it as the first
    /// chat turn.
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    /// Strict-JSON output schema.  Engine-specific; passed through
    /// verbatim to the backend (e.g. llama.cpp's `--grammar` JSON).
    #[serde(default)]
    pub json_schema: Option<serde_json::Value>,
    /// Reasoning effort hint.  Currently honoured by Gemini-style
    /// backends only; ignored elsewhere.
    #[serde(default)]
    pub reasoning: Option<String>,
}

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f32 {
    0.7
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSttParams {
    /// HTTPS URL to fetch the audio bytes from (e.g. R2 signed URL).
    pub input_url: String,
    #[serde(default)]
    pub language: Option<String>,
    /// Translate the audio to English (whisper `--translate`).
    #[serde(default)]
    pub translate: Option<bool>,
    /// Initial prompt to bias the transcription.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Voice-activity detection.
    #[serde(default)]
    pub vad: Option<bool>,
    /// Timestamp granularity: `"segment"` or `"word"`.
    #[serde(default)]
    pub timestamps: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioTtsParams {
    pub text: String,
    #[serde(default = "default_voice")]
    pub voice: String,
    /// Playback speed multiplier (1.0 = natural pace).
    #[serde(default)]
    pub speed: Option<f32>,
    /// Spoken-language hint (e.g. `"en"`, `"nl"`).
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default = "default_audio_ext")]
    pub ext: String,
}

fn default_voice() -> String {
    "default".into()
}
fn default_audio_ext() -> String {
    "wav".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoParams {
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: Option<String>,
    /// HTTPS URL to a base frame for image-to-video models.
    #[serde(default)]
    pub init_image_url: Option<String>,
    #[serde(default = "default_video_seconds")]
    pub seconds: f32,
    /// Frame rate; defaults to backend-specific value when `None`.
    #[serde(default)]
    pub fps: Option<u32>,
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
// ModelSource — download spec the studio attaches to every offer so
// the worker can fetch + run any model without per-model knowledge.
// Mirrors `WorkerModelSource` on the TS side.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelFileRole {
    DiffusionModel,
    TextEncoder,
    /// Vision tower / mmproj for a multimodal text encoder (e.g.
    /// Qwen2.5-VL ViT). Maps to `sd-cli --llm_vision`; required by
    /// instruction-edit models that condition on the reference image.
    TextEncoderVision,
    Vae,
    Lora,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelEngine {
    SdCpp,
    LlamaCpp,
    /// ONNX Runtime image engine (pykeio/ort).  Serves the LaMa
    /// object-removal model used by Find-the-Differences removals.
    Onnx,
    Synthetic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFile {
    pub role: ModelFileRole,
    pub url: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approx_bytes: Option<u64>,
    /// Hex sha256 of the file's bytes.  When present the worker
    /// verifies the downloaded body against it before committing the
    /// file to the cache; absent (legacy registry rows) means
    /// Content-Length is the only integrity check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCliDefaults {
    pub cfg_scale: f32,
    pub steps: u32,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_method: Option<String>,
    /// `--flow-shift` for Flow models (SD3.x / WAN / Qwen-Image). Model-level constant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_shift: Option<f32>,
    /// `--qwen-image-zero-cond-t` — mandatory for Qwen-Image edit quality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zero_cond_t: Option<bool>,
    /// `--offload-to-cpu` — keep weights in RAM, stream to VRAM, so a large model fits a small card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_to_cpu: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSource {
    pub engine: ModelEngine,
    pub files: Vec<ModelFile>,
    pub cli_defaults: ModelCliDefaults,
}

// ---------------------------------------------------------------------------
// JobClaim — every offer carries `task` + `model_source` populated by the
// studio's registry resolver.  No legacy `prompt + ext` shadow fields.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobClaim {
    pub job_id: String,
    #[allow(dead_code)]
    pub game_id: String,
    pub asset_name: String,
    pub model: String,
    pub vram_gb_estimate: f32,
    /// Structured task payload.  Required — the studio refuses to
    /// promote a job without one.  Worker treats a missing `task`
    /// as a protocol_violation.
    pub task: Task,
    /// Download + engine + CLI defaults the studio resolved from its
    /// model registry.  Required — `synthetic` is just another engine
    /// option, not a fallback for missing rows.
    pub model_source: ModelSource,
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

    fn synthetic_model_source_json() -> serde_json::Value {
        serde_json::json!({
            "engine": "synthetic",
            "files": [],
            "cliDefaults": {
                "cfgScale": 1.0,
                "steps": 8,
                "width": 1024,
                "height": 1024,
            },
        })
    }

    #[test]
    fn job_claim_requires_task_and_model_source() {
        // Without `task` or `modelSource` the offer must fail to
        // deserialise — no silent fallback to an Option/Image default.
        let bare = serde_json::json!({
            "jobId": "j-1",
            "gameId": "g-1",
            "assetName": "g-1/creatures/x",
            "model": "synthetic-image",
            "vramGbEstimate": 1.0,
        });
        assert!(
            serde_json::from_value::<JobClaim>(bare).is_err(),
            "JobClaim must reject missing task + modelSource"
        );
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
                "maxTokens": 32,
                "temperature": 0.5,
            },
            "modelSource": synthetic_model_source_json(),
        });
        let claim: JobClaim = serde_json::from_value(json).unwrap();
        match claim.task {
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
            "model": "synthetic-image",
            "vramGbEstimate": 8.0,
            "task": {
                "kind": "image",
                "prompt": "a koi",
                "width": 1024,
                "height": 1024,
                "steps": 30,
                "ext": "png",
            },
            "modelSource": synthetic_model_source_json(),
        });
        let claim: JobClaim = serde_json::from_value(json).unwrap();
        match claim.task {
            Task::Image(p) => {
                assert_eq!(p.prompt, "a koi");
                assert_eq!(p.width, 1024);
                assert_eq!(p.ext, "png");
            }
            other => panic!("expected image, got {:?}", other),
        }
    }

    #[test]
    fn image_params_round_trips_with_new_fields() {
        let json = serde_json::json!({
            "kind": "image",
            "prompt": "a stone golem",
            "negativePrompt": "text, watermark, low quality",
            "initImageUrl": "https://example.invalid/t2-golem-stone/latest.webp",
            "denoise": 0.55,
            "cfgScale": 7.5,
            "samplingMethod": "dpm++2m",
            "width": 768,
            "height": 512,
            "steps": 30,
            "seed": 1234,
            "ext": "webp",
        });
        let task: Task = serde_json::from_value(json).unwrap();
        match task {
            Task::Image(p) => {
                assert_eq!(p.prompt, "a stone golem");
                assert_eq!(
                    p.negative_prompt.as_deref(),
                    Some("text, watermark, low quality")
                );
                assert_eq!(
                    p.init_image_url.as_deref(),
                    Some("https://example.invalid/t2-golem-stone/latest.webp")
                );
                assert!((p.denoise.unwrap() - 0.55).abs() < 1e-6);
                assert!((p.cfg_scale.unwrap() - 7.5).abs() < 1e-6);
                assert_eq!(p.sampling_method.as_deref(), Some("dpm++2m"));
                assert_eq!(p.width, 768);
                assert_eq!(p.height, 512);
                assert_eq!(p.steps, 30);
                assert_eq!(p.seed, Some(1234));
            }
            other => panic!("expected image, got {:?}", other),
        }
    }

    #[test]
    fn image_params_defaults_when_optional_fields_absent() {
        let json = serde_json::json!({
            "kind": "image",
            "prompt": "a fox"
        });
        let task: Task = serde_json::from_value(json).unwrap();
        match task {
            Task::Image(p) => {
                assert_eq!(p.prompt, "a fox");
                assert!(p.negative_prompt.is_none());
                assert!(p.init_image_url.is_none());
                assert!(p.denoise.is_none());
                assert!(p.cfg_scale.is_none());
                assert!(p.sampling_method.is_none());
                assert_eq!(p.width, 512);
                assert_eq!(p.height, 512);
                assert_eq!(p.steps, 20);
                assert_eq!(p.ext, "webp");
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
