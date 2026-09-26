//! What the tray UI can see and do in the daemon, independent of HTTP.
//!
//! The local API's `/daemon/*` routes are thin adapters over
//! [`DaemonControl`]; keeping the logic here makes every rule testable
//! without a socket.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::auto_register::{RegistrationState, SharedRegistration};
use crate::config::SharedConfig;
use crate::daemon_api::{ConfigRejection, DaemonStatus, EditableConfig, JobWire};
use crate::runtime::WorkerObservers;

const TRACE_TARGET: &str = "studio_worker::local_api";

/// The daemon's shared runtime handles.  Cheap to clone.
#[derive(Clone)]
pub struct DaemonControl {
    pub cfg: SharedConfig,
    pub config_path: PathBuf,
    /// Runtime pause toggle (never persisted).
    pub paused: Arc<AtomicBool>,
    /// Set to stop the daemon gracefully.
    pub stop: Arc<AtomicBool>,
    pub registration: SharedRegistration,
    /// Raised by [`DaemonControl::request_registration_reset`]; consumed by
    /// the rejection wait in `runtime::serve_studio`.
    pub reset_requested: Arc<AtomicBool>,
    /// Total device memory, probed once at start.
    pub vram_total_gb: f32,
}

/// Why a control action was refused.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("invalid config: {0}")]
    Invalid(#[from] ConfigRejection),
    #[error("config could not be saved: {0}")]
    NotSaved(String),
    #[error("registration is not rejected; nothing to reset")]
    NotRejected,
}

impl DaemonControl {
    /// Handles with fresh flags for `cfg` stored at `config_path`.
    pub fn new(cfg: SharedConfig, config_path: PathBuf, vram_total_gb: f32) -> Self {
        Self {
            cfg,
            config_path,
            paused: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            registration: crate::auto_register::shared_initial(),
            reset_requested: Arc::new(AtomicBool::new(false)),
            vram_total_gb,
        }
    }

    /// The snapshot `GET /daemon/status` answers.
    pub fn status(&self, observers: &WorkerObservers, busy: bool) -> DaemonStatus {
        let cfg = self.cfg.lock().clone();
        let thumbs = &observers.thumbnails;
        let running = observers
            .active_jobs
            .lock()
            .iter()
            .map(|j| JobWire::running(j, thumbs.contains(&j.job_id)))
            .collect();
        let finished = |ring: &parking_lot::Mutex<std::collections::VecDeque<_>>| {
            ring.lock()
                .iter()
                .map(|j: &crate::runtime::RecentJob| {
                    JobWire::finished(j, thumbs.contains(&j.job_id))
                })
                .collect()
        };
        DaemonStatus {
            version: crate::AGENT_VERSION.to_string(),
            pid: std::process::id(),
            config_path: self.config_path.clone(),
            paused: self.paused.load(Ordering::SeqCst),
            busy,
            registered: cfg.worker_id.is_some() && cfg.auth_token.is_some(),
            worker_id: cfg.worker_id.clone(),
            registration: self.registration.lock().clone(),
            config: EditableConfig::from_config(&cfg),
            session: observers.session_state.lock().clone(),
            heartbeat: observers.last_heartbeat.lock().clone(),
            gpu_runtime: observers.gpu_runtime.lock().clone(),
            vram_total_gb: self.vram_total_gb,
            local_api_url: observers.local_api_url.lock().clone(),
            current_job_id: observers
                .current_job
                .lock()
                .as_ref()
                .map(|j| j.job_id.clone()),
            active_jobs: running,
            recent_jobs: finished(&observers.recent_jobs),
            local_jobs: finished(&observers.local_jobs),
            logs_seq: observers.recent_logs_seq.load(Ordering::SeqCst),
        }
    }

    /// Pause or resume claiming studio jobs.  Answers the new state.
    pub fn set_paused(&self, paused: bool) -> bool {
        let was = self.paused.swap(paused, Ordering::SeqCst);
        tracing::info!(
            target: TRACE_TARGET,
            op = "control",
            action = if paused { "pause" } else { "resume" },
            changed = was != paused,
            "pause toggled through the local api"
        );
        paused
    }

    /// The operator-editable config.
    pub fn editable_config(&self) -> EditableConfig {
        EditableConfig::from_config(&self.cfg.lock())
    }

    /// Validate `edit`, save it, then apply it to the running daemon.
    /// Nothing changes when validation or the save fails.
    pub fn update_config(&self, edit: EditableConfig) -> Result<EditableConfig, ControlError> {
        if let Err(rejection) = edit.validate() {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "control",
                action = "config",
                field = rejection.field,
                problem = %rejection.problem,
                "config update refused"
            );
            return Err(rejection.into());
        }
        let current = self.cfg.lock().clone();
        let mut next = current.clone();
        edit.apply_to(&mut next);
        if let Err(e) = crate::config::save(&next, &self.config_path) {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "control",
                action = "config",
                error = %e,
                "config update could not be saved"
            );
            return Err(ControlError::NotSaved(e.to_string()));
        }
        let changed = crate::config::changed_fields(&current, &next).join(",");
        *self.cfg.lock() = next;
        tracing::info!(
            target: TRACE_TARGET,
            op = "control",
            action = "config",
            changed = %changed,
            "config updated through the local api"
        );
        Ok(self.editable_config())
    }

    /// Ask the daemon to clear a rejected registration and request again.
    pub fn request_registration_reset(&self) -> Result<(), ControlError> {
        if !matches!(
            *self.registration.lock(),
            RegistrationState::Rejected { .. }
        ) {
            tracing::info!(
                target: TRACE_TARGET,
                op = "control",
                action = "registration_reset",
                "registration reset refused: not rejected"
            );
            return Err(ControlError::NotRejected);
        }
        self.reset_requested.store(true, Ordering::SeqCst);
        tracing::info!(
            target: TRACE_TARGET,
            op = "control",
            action = "registration_reset",
            "registration reset requested"
        );
        Ok(())
    }

    /// Stop the daemon gracefully.
    pub fn shutdown(&self) {
        tracing::info!(
            target: TRACE_TARGET,
            op = "control",
            action = "shutdown",
            "shutdown requested through the local api"
        );
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::runtime::{CurrentJob, JobOutcome, JobSource, RecentJob};
    use crate::types::TaskKind;
    use chrono::Utc;

    fn control_in(dir: &std::path::Path) -> DaemonControl {
        let cfg = Config {
            worker_id: Some("w-1".into()),
            auth_token: Some("secret-token".into()),
            ..Config::default()
        };
        DaemonControl::new(crate::config::shared(cfg), dir.join("config.toml"), 24.0)
    }

    #[test]
    fn the_status_carries_jobs_state_and_no_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_in(dir.path());
        let observers = WorkerObservers::default();
        let now = Utc::now();
        observers.active_jobs.lock().push(CurrentJob {
            job_id: "run-1".into(),
            kind: TaskKind::Llm,
            model: "m".into(),
            prompt: "p".into(),
            started_at: now,
            source: JobSource::Lane,
        });
        observers.local_jobs.lock().push_front(RecentJob {
            job_id: "img-1".into(),
            kind: TaskKind::Image,
            model: "m".into(),
            prompt: "p".into(),
            outcome: JobOutcome::Completed,
            started_at: now,
            finished_at: now,
            source: JobSource::Local,
        });
        observers.thumbnails.insert("img-1", vec![1, 2, 3]);
        control.set_paused(true);

        let status = control.status(&observers, true);

        assert!(status.paused && status.busy && status.registered);
        assert_eq!(status.worker_id.as_deref(), Some("w-1"));
        assert_eq!(status.vram_total_gb, 24.0);
        assert_eq!(status.active_jobs[0].job_id, "run-1");
        assert!(status.local_jobs[0].has_thumbnail);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("secret-token"), "{json}");
        let back: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn a_valid_config_update_is_saved_and_applied() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_in(dir.path());
        let mut edit = control.editable_config();
        edit.vram_threshold_gb = 6.0;

        let logs = crate::test_support::capture({
            let control = control.clone();
            move || {
                control.update_config(edit).unwrap();
            }
        });

        assert_eq!(control.cfg.lock().vram_threshold_gb, 6.0);
        let (saved, _) = crate::config::load(Some(&control.config_path.to_string_lossy())).unwrap();
        assert_eq!(saved.vram_threshold_gb, 6.0);
        assert_eq!(saved.auth_token.as_deref(), Some("secret-token"));
        assert!(logs.contains("changed=vram_threshold_gb"), "{logs}");
    }

    #[test]
    fn an_invalid_config_update_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_in(dir.path());
        let mut edit = control.editable_config();
        edit.api_base_url = "nope".into();
        assert!(matches!(
            control.update_config(edit),
            Err(ControlError::Invalid(_))
        ));
        assert_eq!(
            control.cfg.lock().api_base_url,
            Config::default().api_base_url
        );
        assert!(!control.config_path.exists());
    }

    #[test]
    fn a_config_that_cannot_be_saved_is_not_applied() {
        let control = DaemonControl::new(
            crate::config::shared(Config::default()),
            PathBuf::from("/proc/no-such-dir/config.toml"),
            0.0,
        );
        let mut edit = control.editable_config();
        edit.vram_threshold_gb = 3.0;
        assert!(matches!(
            control.update_config(edit),
            Err(ControlError::NotSaved(_))
        ));
        assert_ne!(control.cfg.lock().vram_threshold_gb, 3.0);
    }

    #[test]
    fn a_registration_reset_needs_a_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_in(dir.path());
        assert!(matches!(
            control.request_registration_reset(),
            Err(ControlError::NotRejected)
        ));
        *control.registration.lock() = RegistrationState::Rejected {
            reason: "no".into(),
        };
        control.request_registration_reset().unwrap();
        assert!(control.reset_requested.load(Ordering::SeqCst));
    }

    #[test]
    fn shutdown_raises_the_stop_flag() {
        let dir = tempfile::tempdir().unwrap();
        let control = control_in(dir.path());
        control.shutdown();
        assert!(control.stop.load(Ordering::SeqCst));
    }
}
