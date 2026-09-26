//! Carries the operator's clicks to the daemon off the UI thread, and keeps
//! the one-line result the status line shows.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eframe::egui;
use parking_lot::Mutex;

use crate::daemon_link::{perform, Action, Replica};

/// The result of the last action.
#[derive(Debug, Clone, PartialEq)]
pub struct Feedback {
    pub text: String,
    pub ok: bool,
    pub at: DateTime<Utc>,
}

/// Runs [`Action`]s on background threads.  Cheap to clone.
#[derive(Clone)]
pub struct ActionRunner {
    config_path: PathBuf,
    replica: Replica,
    pub feedback: Arc<Mutex<Option<Feedback>>>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl ActionRunner {
    pub fn new(config_path: PathBuf, replica: Replica) -> Self {
        Self {
            config_path,
            replica,
            feedback: Arc::default(),
            ctx: Arc::default(),
        }
    }

    /// Repaint `ctx` when an action finishes.
    pub fn attach(&self, ctx: egui::Context) {
        *self.ctx.lock() = Some(ctx);
    }

    /// Run `action` on a background thread.
    pub fn run(&self, action: Action) {
        let runner = self.clone();
        std::thread::spawn(move || runner.run_blocking(&action));
    }

    /// Run `action` on a helper thread and wait for it.  The UI thread sits
    /// inside the tokio runtime, where the blocking HTTP client must not
    /// run.
    pub fn run_and_wait(&self, action: Action) -> bool {
        let runner = self.clone();
        std::thread::spawn(move || runner.run_blocking(&action))
            .join()
            .unwrap_or(false)
    }

    /// Run `action` here and record its feedback.
    pub fn run_blocking(&self, action: &Action) -> bool {
        let outcome = perform(&self.config_path, action);
        if let (Ok(_), Action::SetPaused(paused)) = (&outcome, action) {
            // Show the new state at once; the next poll confirms it.
            self.replica.paused.store(*paused, Ordering::SeqCst);
        }
        let ok = outcome.is_ok();
        *self.feedback.lock() = Some(Feedback {
            text: outcome.unwrap_or_else(|e| e),
            ok,
            at: Utc::now(),
        });
        if let Some(ctx) = self.ctx.lock().as_ref() {
            ctx.request_repaint();
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::DaemonHarness;

    #[test]
    fn an_action_records_its_feedback_and_mirrors_pause() {
        let daemon = DaemonHarness::start();
        let replica = Replica::default();
        let runner = ActionRunner::new(daemon.config_path.clone(), replica.clone());
        assert!(runner.run_blocking(&Action::SetPaused(true)));
        assert!(replica.paused.load(Ordering::SeqCst));
        let feedback = runner.feedback.lock().clone().unwrap();
        assert!(feedback.ok);
        assert_eq!(feedback.text, "paused");
    }

    #[test]
    fn run_and_wait_works_from_inside_a_tokio_runtime() {
        let daemon = DaemonHarness::start();
        let runner = ActionRunner::new(daemon.config_path.clone(), Replica::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ok = rt.block_on(async { runner.run_and_wait(Action::SetPaused(true)) });
        assert!(ok);
        assert!(daemon.control.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn a_refusal_is_recorded_as_failed_feedback() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ActionRunner::new(dir.path().join("config.toml"), Replica::default());
        assert!(!runner.run_blocking(&Action::Shutdown));
        let feedback = runner.feedback.lock().clone().unwrap();
        assert!(!feedback.ok);
        assert!(feedback.text.contains("not reachable"), "{}", feedback.text);
    }
}
