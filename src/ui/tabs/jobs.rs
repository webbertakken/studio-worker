//! Jobs tab — the current job in flight + the recent-jobs ring.

use chrono::{DateTime, Utc};
use eframe::egui;

use crate::runtime::{CurrentJob, JobOutcome, RecentJob, WorkerObservers};

/// Pure-data view of the Jobs tab.  Built each frame from the
/// observers; no egui types in scope.
#[derive(Debug, Clone, PartialEq)]
pub struct JobsView {
    pub current: Option<JobCard>,
    pub recent: Vec<JobCard>,
    /// Jobs submitted to the always-on local API (the local queue).
    pub local: Vec<JobCard>,
    /// URL the local API is reachable at, if it bound.
    pub local_api_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobCard {
    pub job_id: String,
    pub kind: String,
    pub model: String,
    pub prompt: String,
    pub state: JobCardState,
    pub started_at: DateTime<Utc>,
    /// Set on recent jobs only.
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobCardState {
    InFlight,
    Completed,
    Failed(String),
}

impl JobsView {
    pub fn build(observers: &WorkerObservers, now: DateTime<Utc>) -> Self {
        let current = observers
            .current_job
            .lock()
            .as_ref()
            .map(|j| JobCard::from_current(j, now));
        let recent: Vec<JobCard> = observers
            .recent_jobs
            .lock()
            .iter()
            .map(JobCard::from_recent)
            .collect();
        let local: Vec<JobCard> = observers
            .local_jobs
            .lock()
            .iter()
            .map(JobCard::from_recent)
            .collect();
        let local_api_url = observers.local_api_url.lock().clone();
        Self {
            current,
            recent,
            local,
            local_api_url,
        }
    }
}

impl JobCard {
    fn from_current(j: &CurrentJob, _now: DateTime<Utc>) -> Self {
        Self {
            job_id: j.job_id.clone(),
            kind: j.kind.as_str().to_string(),
            model: j.model.clone(),
            prompt: j.prompt.clone(),
            state: JobCardState::InFlight,
            started_at: j.started_at,
            finished_at: None,
        }
    }

    fn from_recent(r: &RecentJob) -> Self {
        let state = match &r.outcome {
            JobOutcome::Completed => JobCardState::Completed,
            JobOutcome::Failed { reason } => JobCardState::Failed(reason.clone()),
        };
        Self {
            job_id: r.job_id.clone(),
            kind: r.kind.as_str().to_string(),
            model: r.model.clone(),
            prompt: r.prompt.clone(),
            state,
            started_at: r.started_at,
            finished_at: Some(r.finished_at),
        }
    }

    pub fn elapsed(&self, now: DateTime<Utc>) -> chrono::Duration {
        let end = self.finished_at.unwrap_or(now);
        end.signed_duration_since(self.started_at)
    }
}

/// Render a `chrono::Duration` as `12s` / `3m 04s` / `1h 12m`.
pub fn format_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        let rem = secs % 60;
        return format!("{mins}m {rem:02}s");
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    format!("{hours}h {rem_min:02}m")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render(ui: &mut egui::Ui, view: &JobsView) {
    let now = Utc::now();

    ui.heading("Current");
    ui.add_space(4.0);
    match &view.current {
        None => {
            ui.label(egui::RichText::new("No job running.").italics());
        }
        Some(card) => render_card(ui, card, now, true),
    }
    ui.add_space(16.0);

    ui.heading(format!("Recent ({})", view.recent.len()));
    ui.add_space(4.0);
    if view.recent.is_empty() {
        ui.label(egui::RichText::new("No completed jobs yet.").italics());
    } else {
        for card in &view.recent {
            render_card(ui, card, now, false);
            ui.add_space(4.0);
        }
    }

    ui.add_space(16.0);
    ui.heading(format!("Local queue ({})", view.local.len()));
    if let Some(url) = &view.local_api_url {
        ui.label(
            egui::RichText::new(format!("API: {url}"))
                .color(egui::Color32::from_gray(140))
                .small(),
        );
    }
    ui.add_space(4.0);
    if view.local.is_empty() {
        ui.label(egui::RichText::new("No local jobs yet.").italics());
    } else {
        for card in &view.local {
            render_card(ui, card, now, false);
            ui.add_space(4.0);
        }
    }
}

fn render_card(ui: &mut egui::Ui, card: &JobCard, now: DateTime<Utc>, emphasised: bool) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (label, colour) = match &card.state {
                JobCardState::InFlight => ("RUNNING", egui::Color32::from_rgb(232, 168, 56)),
                JobCardState::Completed => ("OK", egui::Color32::LIGHT_GREEN),
                JobCardState::Failed(_) => ("FAIL", egui::Color32::LIGHT_RED),
            };
            let badge = egui::RichText::new(label).color(colour).strong();
            if emphasised {
                ui.label(badge.size(14.0));
            } else {
                ui.label(badge);
            }
            ui.label("\u{2014}");
            ui.monospace(&card.kind);
            ui.label("\u{00b7}");
            ui.monospace(&card.model);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format_duration(card.elapsed(now)));
            });
        });
        if !card.prompt.is_empty() {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(&card.prompt)
                    .italics()
                    .color(egui::Color32::from_gray(180)),
            );
        }
        if let JobCardState::Failed(reason) = &card.state {
            ui.add_space(2.0);
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 120),
                format!("reason: {reason}"),
            );
        }
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(format!("job {}", card.job_id))
                .color(egui::Color32::from_gray(140))
                .small(),
        );
    });
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::runtime::{CurrentJob, JobOutcome, RecentJob, WorkerObservers};
    use crate::types::TaskKind;

    fn empty_observers() -> WorkerObservers {
        WorkerObservers {
            current_job: Arc::new(Mutex::new(None)),
            recent_jobs: Arc::new(Mutex::new(VecDeque::new())),
            local_jobs: Arc::new(Mutex::new(VecDeque::new())),
            local_api_url: Arc::new(Mutex::new(None)),
            last_heartbeat: Arc::new(Mutex::new(None)),
            recent_logs: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    #[test]
    fn build_empty_view_when_observers_empty() {
        let view = JobsView::build(&empty_observers(), Utc::now());
        assert!(view.current.is_none());
        assert!(view.recent.is_empty());
    }

    #[test]
    fn build_renders_current_job_when_present() {
        let observers = empty_observers();
        *observers.current_job.lock() = Some(CurrentJob {
            job_id: "j-x".into(),
            kind: TaskKind::Image,
            model: "synthetic".into(),
            prompt: "a tree".into(),
            started_at: Utc::now(),
        });
        let view = JobsView::build(&observers, Utc::now());
        let current = view.current.expect("current must be Some");
        assert_eq!(current.job_id, "j-x");
        assert_eq!(current.kind, "image");
        assert_eq!(current.prompt, "a tree");
        assert!(matches!(current.state, JobCardState::InFlight));
        assert!(current.finished_at.is_none());
    }

    #[test]
    fn build_renders_recent_jobs_newest_first() {
        let observers = empty_observers();
        {
            let mut ring = observers.recent_jobs.lock();
            let now = Utc::now();
            ring.push_front(RecentJob {
                job_id: "j-1".into(),
                kind: TaskKind::Image,
                model: "synthetic".into(),
                prompt: "p1".into(),
                outcome: JobOutcome::Completed,
                started_at: now,
                finished_at: now,
            });
            ring.push_front(RecentJob {
                job_id: "j-2".into(),
                kind: TaskKind::Llm,
                model: "synthetic-llm".into(),
                prompt: "p2".into(),
                outcome: JobOutcome::Failed {
                    reason: "boom".into(),
                },
                started_at: now,
                finished_at: now,
            });
        }
        let view = JobsView::build(&empty_observers_with_data(observers), Utc::now());
        assert_eq!(view.recent.len(), 2);
        assert_eq!(view.recent[0].job_id, "j-2");
        assert!(matches!(view.recent[0].state, JobCardState::Failed(_)));
        assert_eq!(view.recent[1].job_id, "j-1");
        assert!(matches!(view.recent[1].state, JobCardState::Completed));
    }

    // Helper: pass-through so the borrow above doesn't fight with the
    // build() call.
    fn empty_observers_with_data(o: WorkerObservers) -> WorkerObservers {
        o
    }

    #[test]
    fn build_includes_local_jobs_and_url() {
        let observers = empty_observers();
        *observers.local_api_url.lock() = Some("http://127.0.0.1:4787".into());
        {
            let now = Utc::now();
            observers.local_jobs.lock().push_front(RecentJob {
                job_id: "local-1".into(),
                kind: TaskKind::Image,
                model: "z-image-turbo-q4_k_m.gguf".into(),
                prompt: "a fox".into(),
                outcome: JobOutcome::Completed,
                started_at: now,
                finished_at: now,
            });
        }
        let view = JobsView::build(&empty_observers_with_data(observers), Utc::now());
        assert_eq!(view.local.len(), 1);
        assert_eq!(view.local[0].job_id, "local-1");
        assert_eq!(view.local_api_url.as_deref(), Some("http://127.0.0.1:4787"));
        // Local jobs do not leak into the studio recent ring.
        assert!(view.recent.is_empty());
    }

    #[test]
    fn format_duration_sub_minute() {
        assert_eq!(
            format_duration(chrono::Duration::seconds(12)),
            "12s".to_string()
        );
    }

    #[test]
    fn format_duration_sub_hour() {
        assert_eq!(
            format_duration(chrono::Duration::seconds(184)),
            "3m 04s".to_string()
        );
    }

    #[test]
    fn format_duration_multi_hour() {
        assert_eq!(
            format_duration(chrono::Duration::seconds(3600 + 12 * 60)),
            "1h 12m".to_string()
        );
    }

    #[test]
    fn format_duration_negative_clamps_to_zero() {
        assert_eq!(
            format_duration(chrono::Duration::seconds(-5)),
            "0s".to_string()
        );
    }
}
