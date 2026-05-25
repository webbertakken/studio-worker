//! OS-native desktop notifications gated on per-event toggles.
//!
//! `Notifier` is a trait so tests can substitute a `CapturingNotifier`
//! and assert what would have been shown without invoking the real
//! `notify-rust` D-Bus / NSUserNotification / WinRT path.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::runtime::{JobOutcome, RecentJob};

/// Pair of toggles read out of `Config` (Phase 6 surfaces them in the
/// Config tab; for now we hold them on the App and persist via the
/// same draft path as everything else).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NotificationPrefs {
    pub on_completion: bool,
    pub on_failure: bool,
}

pub trait Notifier {
    fn show(&self, title: &str, body: &str);
}

#[derive(Default)]
pub struct CapturingNotifier {
    pub captured: Arc<Mutex<Vec<(String, String)>>>,
}

impl Notifier for CapturingNotifier {
    fn show(&self, title: &str, body: &str) {
        self.captured.lock().push((title.into(), body.into()));
    }
}

#[cfg(feature = "ui")]
pub struct DesktopNotifier;

#[cfg(feature = "ui")]
impl Notifier for DesktopNotifier {
    fn show(&self, title: &str, body: &str) {
        if let Err(e) = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .appname("studio-worker")
            .show()
        {
            tracing::warn!(
                target: "studio_worker::ui::notifier",
                "notification failed: {e}"
            );
        }
    }
}

/// Decision the gate makes for a single recent-job entry.  Pure data
/// so we can assert on it without running a real notifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyDecision {
    Skip,
    Show { title: String, body: String },
}

pub fn decide(prefs: NotificationPrefs, recent: &RecentJob) -> NotifyDecision {
    let allow = match &recent.outcome {
        JobOutcome::Completed => prefs.on_completion,
        JobOutcome::Failed { .. } => prefs.on_failure,
    };
    if !allow {
        return NotifyDecision::Skip;
    }
    let title = match &recent.outcome {
        JobOutcome::Completed => "studio-worker — job completed".into(),
        JobOutcome::Failed { .. } => "studio-worker — job failed".into(),
    };
    let body = match &recent.outcome {
        JobOutcome::Completed => format!("{} · {}", recent.kind.as_str(), recent.model),
        JobOutcome::Failed { reason } => {
            format!("{} · {} — {reason}", recent.kind.as_str(), recent.model)
        }
    };
    NotifyDecision::Show { title, body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::JobOutcome;
    use crate::types::TaskKind;
    use chrono::Utc;

    fn completed_job() -> RecentJob {
        let now = Utc::now();
        RecentJob {
            job_id: "j-1".into(),
            kind: TaskKind::Image,
            model: "synthetic".into(),
            prompt: "a tree".into(),
            outcome: JobOutcome::Completed,
            started_at: now,
            finished_at: now,
        }
    }

    fn failed_job(reason: &str) -> RecentJob {
        let mut j = completed_job();
        j.outcome = JobOutcome::Failed {
            reason: reason.into(),
        };
        j
    }

    #[test]
    fn decide_skips_both_when_prefs_disabled() {
        let prefs = NotificationPrefs::default();
        assert_eq!(decide(prefs, &completed_job()), NotifyDecision::Skip);
        assert_eq!(decide(prefs, &failed_job("x")), NotifyDecision::Skip);
    }

    #[test]
    fn decide_emits_completion_when_toggle_on() {
        let prefs = NotificationPrefs {
            on_completion: true,
            on_failure: false,
        };
        match decide(prefs, &completed_job()) {
            NotifyDecision::Show { title, body } => {
                assert!(title.contains("completed"));
                assert!(body.contains("image"));
                assert!(body.contains("synthetic"));
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn decide_emits_failure_with_reason() {
        let prefs = NotificationPrefs {
            on_completion: false,
            on_failure: true,
        };
        match decide(prefs, &failed_job("boom")) {
            NotifyDecision::Show { title, body } => {
                assert!(title.contains("failed"));
                assert!(body.contains("boom"));
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn capturing_notifier_records_calls() {
        let n = CapturingNotifier::default();
        n.show("t", "b");
        n.show("t2", "b2");
        let captured = n.captured.lock();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[1], ("t2".into(), "b2".into()));
    }
}
