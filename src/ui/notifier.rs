//! OS-native desktop notifications gated on per-event toggles.
//!
//! `Notifier` is a trait so tests can substitute a `CapturingNotifier`
//! and assert what would have been shown without invoking the real
//! `notify-rust` D-Bus / NSUserNotification / WinRT path.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::runtime::{JobOutcome, RecentJob};

/// Per-event desktop-notification toggles.  Surfaced in the Config
/// tab and held on the `App` for the current session only.  They are
/// not part of the persisted `Config`, so they reset to off on each
/// restart.
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

/// Tracing target for desktop-notification events.  Stable so
/// operators can filter with
/// `RUST_LOG=studio_worker::ui::notifier=debug`.
const TRACE_TARGET: &str = "studio_worker::ui::notifier";

/// Emit a structured breadcrumb for a desktop-notification attempt.
/// Pulled out of [`DesktopNotifier::show`] so both branches are
/// observable AND unit-testable without a real D-Bus / WinRT /
/// NSUserNotification round-trip.  Success logs at `debug` so an
/// operator can confirm the notifier actually fired; failure logs at
/// `warn` with a structured `error` field, matching the rest of the
/// worker's logging convention (the old inline call swallowed the
/// success case entirely and string-interpolated the error).
fn log_show_outcome(title: &str, result: Result<(), String>) {
    match result {
        Ok(()) => tracing::debug!(
            target: TRACE_TARGET,
            op = "show",
            title = %title,
            "desktop notification shown"
        ),
        Err(e) => tracing::warn!(
            target: TRACE_TARGET,
            op = "show",
            title = %title,
            error = %e,
            "desktop notification failed"
        ),
    }
}

#[cfg(feature = "ui")]
pub struct DesktopNotifier;

#[cfg(feature = "ui")]
impl Notifier for DesktopNotifier {
    fn show(&self, title: &str, body: &str) {
        let result = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .appname("studio-worker")
            .show()
            .map(|_| ())
            .map_err(|e| e.to_string());
        log_show_outcome(title, result);
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

    // -----------------------------------------------------------------
    // log_show_outcome — the structured breadcrumb the real
    // `DesktopNotifier` emits.  Without these, a desktop notification
    // that silently fails (no D-Bus session on a headless box) or one
    // that fired correctly leaves no operator-visible trail, and the
    // logging diverges from the rest of the worker's `target` / `op` /
    // `error` convention.
    // -----------------------------------------------------------------

    #[test]
    fn log_show_outcome_emits_debug_breadcrumb_on_success() {
        let logs = crate::test_support::capture(|| {
            log_show_outcome("studio-worker \u{2014} job completed", Ok(()));
        });
        assert!(logs.contains("DEBUG"), "expected DEBUG level, got: {logs}");
        assert!(
            logs.contains("studio_worker::ui::notifier"),
            "expected notifier target, got: {logs}"
        );
        assert!(logs.contains("op=\"show\""), "expected op field: {logs}");
        assert!(
            logs.contains("desktop notification shown"),
            "expected success message: {logs}"
        );
    }

    #[test]
    fn log_show_outcome_emits_warn_with_structured_error_on_failure() {
        let logs = crate::test_support::capture(|| {
            log_show_outcome(
                "studio-worker \u{2014} job failed",
                Err("no d-bus session".into()),
            );
        });
        assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
        // `error = %e` renders via Display (no quotes), matching the
        // worker's `error = %e` logging convention.
        assert!(
            logs.contains("error=no d-bus session"),
            "expected structured error field, got: {logs}"
        );
        assert!(
            logs.contains("desktop notification failed"),
            "expected failure message: {logs}"
        );
    }
}
