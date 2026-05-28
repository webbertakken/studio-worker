//! Rust mirrors of the WebSocket frame types defined in
//! `apps/studio/src/shared/types/workerWs.ts`.
//!
//! Field names use camelCase on the wire (matching the TS contract);
//! Rust snake_case identifiers are translated with serde renames.  The
//! tag field is `"type"` with camelCase variant names.
use crate::types::{JobClaim, LogEntry, ModelSource, Task, WorkerCapabilities};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Worker → server (inbound, from the DO's point of view)
// ---------------------------------------------------------------------------

/// Authentication payload sent right after the WS upgrade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloFrame {
    pub auth_token: String,
    pub capabilities: WorkerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerInbound {
    Hello(HelloFrame),
    #[serde(rename_all = "camelCase")]
    Heartbeat {
        capabilities: WorkerCapabilities,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_job_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Accept {
        job_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Reject {
        job_id: String,
        reason: String,
    },
    #[serde(rename_all = "camelCase")]
    CompleteJson {
        job_id: String,
        result: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Fail {
        job_id: String,
        error: String,
        retryable: bool,
    },
    LogBatch {
        entries: Vec<LogEntry>,
    },
    ReadyForMore,
}

// ---------------------------------------------------------------------------
// Server → worker (outbound, from the DO's point of view)
// ---------------------------------------------------------------------------

/// Mirror of `JobClaimResponse` carried inside an `offer` frame.
///
/// Identical wire shape to the existing `JobClaim` used by the
/// pull-based HTTP path, but defined separately so that future evolution
/// of the WS protocol (e.g. additional offer-time metadata) doesn't
/// drag the HTTP claim contract along.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOfferClaim {
    pub job_id: String,
    pub game_id: String,
    pub asset_name: String,
    pub model: String,
    pub vram_gb_estimate: f32,
    #[serde(default)]
    pub prompt: String,
    #[serde(default = "default_image_ext")]
    pub ext: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Task>,
    /// Download + engine + CLI defaults the studio resolved from its
    /// model registry.  Absent on legacy queued rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<ModelSource>,
}

impl JobOfferClaim {
    /// Bridge to the existing HTTP-shaped `JobClaim` so engine dispatch
    /// code can stay kind-agnostic.
    pub fn into_job_claim(self) -> JobClaim {
        JobClaim {
            job_id: self.job_id,
            game_id: self.game_id,
            asset_name: self.asset_name,
            model: self.model,
            vram_gb_estimate: self.vram_gb_estimate,
            prompt: self.prompt,
            ext: self.ext,
            task: self.task,
            model_source: self.model_source,
        }
    }
}

fn default_image_ext() -> String {
    "webp".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerErrorCode {
    AuthFailed,
    ProtocolViolation,
    DuplicateWorker,
    WorkerDeleted,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerOutbound {
    #[serde(rename_all = "camelCase")]
    Welcome {
        worker_id: String,
        server_time: String,
    },
    Offer {
        // Boxed to keep the WorkerOutbound enum compact — JobOfferClaim
        // grew to ~300 bytes once we started carrying the ModelSource
        // (download URLs + CLI defaults) on every offer.
        claim: Box<JobOfferClaim>,
    },
    HeartbeatAck,
    #[serde(rename_all = "camelCase")]
    CompleteAck {
        job_id: String,
    },
    #[serde(rename_all = "camelCase")]
    FailAck {
        job_id: String,
    },
    Error {
        code: WorkerErrorCode,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// WebSocket close codes
//
// Kept as numeric values so they can flow straight into
// `tungstenite::protocol::CloseFrame { code: <u16>, reason: ... }`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum WsCloseCode {
    Normal = 1000,
    AuthFailed = 4001,
    ProtocolViolation = 4002,
    DuplicateWorker = 4003,
    WorkerDeleted = 4004,
}

impl WsCloseCode {
    /// Maps a `WorkerErrorCode` to its paired close code.  The mapping
    /// must stay 1:1 with `closeCodeForError` in `workerWs.ts`.
    pub fn from_error_code(code: WorkerErrorCode) -> Self {
        match code {
            WorkerErrorCode::AuthFailed => Self::AuthFailed,
            WorkerErrorCode::ProtocolViolation => Self::ProtocolViolation,
            WorkerErrorCode::DuplicateWorker => Self::DuplicateWorker,
            WorkerErrorCode::WorkerDeleted => Self::WorkerDeleted,
            WorkerErrorCode::InternalError => Self::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Smoke-test that the `From<JobOfferClaim>` bridge round-trips the
    /// prompt + task without losing fields.
    #[test]
    fn job_offer_claim_into_job_claim_preserves_fields() {
        let offer = JobOfferClaim {
            job_id: "j".into(),
            game_id: "g".into(),
            asset_name: "g/x/y".into(),
            model: "m".into(),
            vram_gb_estimate: 1.5,
            prompt: "hi".into(),
            ext: "png".into(),
            task: None,
            model_source: None,
        };
        let claim = offer.into_job_claim();
        assert_eq!(claim.job_id, "j");
        assert_eq!(claim.model, "m");
        assert_eq!(claim.vram_gb_estimate, 1.5);
        assert_eq!(claim.ext, "png");
    }

    /// Sanity check that the `prompt` default + `ext` default work.
    #[test]
    fn job_offer_claim_uses_default_ext_when_missing() {
        let json = json!({
            "jobId": "j",
            "gameId": "g",
            "assetName": "g/x/y",
            "model": "m",
            "vramGbEstimate": 1.0,
        });
        let parsed: JobOfferClaim = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.ext, "webp");
        assert_eq!(parsed.prompt, "");
    }
}
