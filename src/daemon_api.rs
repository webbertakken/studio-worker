//! Wire types of the daemon-control routes on the local API, shared by the
//! daemon (which answers them) and the tray UI (which reads them).
//!
//! See `docs/local-api.md#daemon-control` for the routes.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auto_register::RegistrationState;
use crate::config::Config;
use crate::runtime::{
    CurrentJob, GpuRuntimeStatus, HeartbeatStatus, JobOutcome, JobSource, RecentJob, SessionState,
};
use crate::types::{LogEntry, ModelEngine, TaskKind};

/// The operator-editable part of the config: what the Config tab shows and
/// `PUT /daemon/config` accepts.  Credentials and registration state are
/// never part of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditableConfig {
    pub api_base_url: String,
    pub vram_threshold_gb: f32,
    pub start_minimised: bool,
    pub auto_update_enabled: bool,
    pub auto_update_interval_secs: u64,
    pub auto_update_feed: String,
    pub auto_update_prerelease: bool,
    pub models_root: PathBuf,
}

/// Why `PUT /daemon/config` refused a config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {problem}")]
pub struct ConfigRejection {
    pub field: &'static str,
    pub problem: String,
}

/// Shortest auto-update interval accepted, in seconds.  The release feed is
/// GitHub's API; polling it faster than once a minute buys nothing.
pub const MIN_AUTO_UPDATE_INTERVAL_SECS: u64 = 60;

impl EditableConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            api_base_url: cfg.api_base_url.clone(),
            vram_threshold_gb: cfg.vram_threshold_gb,
            start_minimised: cfg.start_minimised,
            auto_update_enabled: cfg.auto_update_enabled,
            auto_update_interval_secs: cfg.auto_update_interval_secs,
            auto_update_feed: cfg.auto_update_feed.clone(),
            auto_update_prerelease: cfg.auto_update_prerelease,
            models_root: cfg.models_root.clone(),
        }
    }

    /// Write these fields onto `cfg`, leaving every other field alone.
    pub fn apply_to(&self, cfg: &mut Config) {
        cfg.api_base_url = self.api_base_url.clone();
        cfg.vram_threshold_gb = self.vram_threshold_gb;
        cfg.start_minimised = self.start_minimised;
        cfg.auto_update_enabled = self.auto_update_enabled;
        cfg.auto_update_interval_secs = self.auto_update_interval_secs;
        cfg.auto_update_feed = self.auto_update_feed.clone();
        cfg.auto_update_prerelease = self.auto_update_prerelease;
        cfg.models_root = self.models_root.clone();
    }

    /// Refuse values the worker cannot run with.
    pub fn validate(&self) -> Result<(), ConfigRejection> {
        let reject = |field, problem: &str| {
            Err(ConfigRejection {
                field,
                problem: problem.to_string(),
            })
        };
        if !is_http_url(&self.api_base_url) {
            return reject("apiBaseUrl", "must be an http(s) URL");
        }
        if !self.vram_threshold_gb.is_finite() || self.vram_threshold_gb < 0.0 {
            return reject("vramThresholdGb", "must be a number of GB, 0 or more");
        }
        if self.auto_update_interval_secs < MIN_AUTO_UPDATE_INTERVAL_SECS {
            return reject("autoUpdateIntervalSecs", "must be at least 60 seconds");
        }
        if !is_http_url(&self.auto_update_feed) {
            return reject("autoUpdateFeed", "must be an http(s) URL");
        }
        if self.models_root.as_os_str().is_empty() {
            return reject("modelsRoot", "must not be empty");
        }
        Ok(())
    }
}

fn is_http_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
}

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
}

/// One job on the wire: running or finished, whatever its source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobWire {
    pub job_id: String,
    pub kind: TaskKind,
    pub model: String,
    pub prompt: String,
    pub source: JobSource,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// `GET /jobs/:id/thumbnail` answers an image.
    #[serde(default)]
    pub has_thumbnail: bool,
}

impl JobWire {
    pub fn running(job: &CurrentJob, has_thumbnail: bool) -> Self {
        Self {
            job_id: job.job_id.clone(),
            kind: job.kind,
            model: job.model.clone(),
            prompt: job.prompt.clone(),
            source: job.source,
            status: JobStatus::Running,
            reason: None,
            started_at: job.started_at,
            finished_at: None,
            has_thumbnail,
        }
    }

    pub fn finished(job: &RecentJob, has_thumbnail: bool) -> Self {
        let (status, reason) = match &job.outcome {
            JobOutcome::Completed => (JobStatus::Completed, None),
            JobOutcome::Failed { reason } => (JobStatus::Failed, Some(reason.clone())),
        };
        Self {
            job_id: job.job_id.clone(),
            kind: job.kind,
            model: job.model.clone(),
            prompt: job.prompt.clone(),
            source: job.source,
            status,
            reason,
            started_at: job.started_at,
            finished_at: Some(job.finished_at),
            has_thumbnail,
        }
    }

    pub fn to_current(&self) -> CurrentJob {
        CurrentJob {
            job_id: self.job_id.clone(),
            kind: self.kind,
            model: self.model.clone(),
            prompt: self.prompt.clone(),
            started_at: self.started_at,
            source: self.source,
        }
    }

    /// The finished job, `None` while it runs.
    pub fn to_recent(&self) -> Option<RecentJob> {
        let outcome = match self.status {
            JobStatus::Running => return None,
            JobStatus::Completed => JobOutcome::Completed,
            JobStatus::Failed => JobOutcome::Failed {
                reason: self.reason.clone().unwrap_or_default(),
            },
        };
        Some(RecentJob {
            job_id: self.job_id.clone(),
            kind: self.kind,
            model: self.model.clone(),
            prompt: self.prompt.clone(),
            outcome,
            started_at: self.started_at,
            finished_at: self.finished_at.unwrap_or(self.started_at),
            source: self.source,
        })
    }
}

/// `GET /daemon/status`: everything the tray UI shows except the logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub config_path: PathBuf,
    pub paused: bool,
    /// The one-job gate is taken (a studio or transient local job runs).
    pub busy: bool,
    /// `worker_id` and `auth_token` are both present.
    pub registered: bool,
    pub worker_id: Option<String>,
    pub registration: RegistrationState,
    pub config: EditableConfig,
    pub session: SessionState,
    pub heartbeat: Option<HeartbeatStatus>,
    pub gpu_runtime: Option<GpuRuntimeStatus>,
    pub vram_total_gb: f32,
    pub local_api_url: Option<String>,
    /// The studio job the heartbeat reports, if any.
    pub current_job_id: Option<String>,
    pub active_jobs: Vec<JobWire>,
    pub recent_jobs: Vec<JobWire>,
    pub local_jobs: Vec<JobWire>,
    /// Sequence number of the newest worker log entry.
    pub logs_seq: u64,
}

/// `GET /daemon/logs?after=<seq>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsPage {
    pub entries: Vec<LogEntry>,
    /// Sequence number of the newest entry; pass it back as `after`.
    pub seq: u64,
}

/// The engine part of a catalogue model's source, as `GET /models` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSourceBrief {
    pub engine: ModelEngine,
}

/// One `GET /models` entry, as the tray UI reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    pub kind: TaskKind,
    #[serde(default)]
    pub vram_gb_estimate: f32,
    pub source: ModelSourceBrief,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub exclusive_group: Option<String>,
    pub state: String,
    #[serde(default)]
    pub resident: bool,
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub error: Option<String>,
}

fn default_true() -> bool {
    true
}

/// An error answer: a stable code plus a human message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editable() -> EditableConfig {
        EditableConfig::from_config(&Config::default())
    }

    #[test]
    fn the_defaults_are_valid() {
        assert_eq!(editable().validate(), Ok(()));
    }

    #[test]
    fn apply_writes_only_the_editable_fields() {
        let mut cfg = Config {
            worker_id: Some("w-1".into()),
            auth_token: Some("secret".into()),
            ..Config::default()
        };
        let mut edit = editable();
        edit.vram_threshold_gb = 7.5;
        edit.models_root = PathBuf::from("/srv/models");
        edit.apply_to(&mut cfg);
        assert_eq!(cfg.vram_threshold_gb, 7.5);
        assert_eq!(cfg.models_root, PathBuf::from("/srv/models"));
        assert_eq!(cfg.worker_id.as_deref(), Some("w-1"));
        assert_eq!(cfg.auth_token.as_deref(), Some("secret"));
        assert_eq!(EditableConfig::from_config(&cfg), edit);
    }

    /// A field name and a way to break it.
    type BrokenField = (&'static str, fn(&mut EditableConfig));

    #[test]
    fn invalid_values_are_refused_with_the_field_named() {
        let cases: Vec<BrokenField> = vec![
            ("apiBaseUrl", |c| c.api_base_url = "not a url".into()),
            ("apiBaseUrl", |c| c.api_base_url = "ftp://x".into()),
            ("vramThresholdGb", |c| c.vram_threshold_gb = -1.0),
            ("vramThresholdGb", |c| c.vram_threshold_gb = f32::NAN),
            ("autoUpdateIntervalSecs", |c| {
                c.auto_update_interval_secs = 5
            }),
            ("autoUpdateFeed", |c| c.auto_update_feed = "".into()),
            ("modelsRoot", |c| c.models_root = PathBuf::new()),
        ];
        for (field, break_it) in cases {
            let mut edit = editable();
            break_it(&mut edit);
            let err = edit.validate().unwrap_err();
            assert_eq!(err.field, field, "{err}");
        }
    }

    fn finished(outcome: JobOutcome) -> RecentJob {
        let now = Utc::now();
        RecentJob {
            job_id: "j-1".into(),
            kind: TaskKind::Image,
            model: "m".into(),
            prompt: "p".into(),
            outcome,
            started_at: now,
            finished_at: now,
            source: JobSource::Lane,
        }
    }

    #[test]
    fn a_finished_job_round_trips_through_the_wire() {
        for outcome in [
            JobOutcome::Completed,
            JobOutcome::Failed {
                reason: "boom".into(),
            },
        ] {
            let job = finished(outcome);
            let wire = JobWire::finished(&job, true);
            let json = serde_json::to_string(&wire).unwrap();
            let back: JobWire = serde_json::from_str(&json).unwrap();
            assert_eq!(back, wire);
            assert_eq!(back.to_recent(), Some(job));
            assert!(back.has_thumbnail);
        }
    }

    #[test]
    fn a_running_job_round_trips_and_is_not_finished() {
        let job = CurrentJob {
            job_id: "j-2".into(),
            kind: TaskKind::Llm,
            model: "m".into(),
            prompt: "p".into(),
            started_at: Utc::now(),
            source: JobSource::Stream,
        };
        let wire = JobWire::running(&job, false);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["status"], "running");
        assert_eq!(json["source"], "stream");
        assert!(json.get("finishedAt").is_none());
        let back: JobWire = serde_json::from_value(json).unwrap();
        assert_eq!(back.to_current(), job);
        assert_eq!(back.to_recent(), None);
    }

    #[test]
    fn runtime_states_round_trip_through_the_wire() {
        let registration = RegistrationState::Pending {
            request_id: "rr-1".into(),
            since: Utc::now(),
        };
        let json = serde_json::to_value(&registration).unwrap();
        assert_eq!(json["state"], "pending");
        assert_eq!(json["requestId"], "rr-1");
        assert_eq!(
            serde_json::from_value::<RegistrationState>(json).unwrap(),
            registration
        );

        let session = SessionState::Reconnecting { attempt: 3 };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "state": "reconnecting", "attempt": 3 })
        );
        assert_eq!(
            serde_json::from_value::<SessionState>(json).unwrap(),
            session
        );

        let heartbeat = HeartbeatStatus {
            outcome: crate::runtime::HeartbeatOutcome::Err {
                reason: "timeout".into(),
            },
            last_attempt_at: Utc::now(),
        };
        let json = serde_json::to_value(&heartbeat).unwrap();
        assert_eq!(json["outcome"], "err");
        assert_eq!(json["reason"], "timeout");
        assert_eq!(
            serde_json::from_value::<HeartbeatStatus>(json).unwrap(),
            heartbeat
        );
    }

    #[test]
    fn a_model_entry_reads_the_models_listing() {
        let json = serde_json::json!({
            "id": "qwen", "displayName": "Qwen", "kind": "llm", "vramGbEstimate": 2.5,
            "source": { "engine": "llama-cpp", "files": [] },
            "enabled": true, "origin": "local",
            "state": "failed", "resident": true,
            "since": "2026-01-01T00:00:00Z", "error": "out of memory"
        });
        let entry: ModelEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry.source.engine, ModelEngine::LlamaCpp);
        assert_eq!(entry.state, "failed");
        assert_eq!(entry.error.as_deref(), Some("out of memory"));
    }
}
