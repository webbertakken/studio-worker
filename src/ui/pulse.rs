//! The worker's pulse: what the header always says.  Pure data built from
//! the replica, so what the header says is tested without egui.

use chrono::{DateTime, Utc};

use crate::auto_register::RegistrationState;
use crate::daemon_api::ModelEntry;
use crate::daemon_link::LinkState;
use crate::runtime::{CurrentJob, SessionState};

use super::format::format_duration;
use super::theme::Tone;

/// What the header shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Pulse {
    pub activity: Activity,
    pub daemon: Signal,
    pub studio: Signal,
    pub gpu: GpuMemory,
    /// The daemon answers, so Pause / Resume can reach it.
    pub can_pause: bool,
    pub paused: bool,
}

/// What the worker is doing.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    /// The daemon does not answer.
    Offline,
    Idle,
    /// Not claiming studio jobs, nothing running.
    Paused,
    /// The one-job gate is taken but no job is listed yet.
    Busy,
    Running {
        kind: String,
        model: String,
        elapsed: String,
        /// Further jobs running besides the one shown.
        more: usize,
    },
}

impl Activity {
    /// The headline, e.g. `Running image · sdxl · 12s`.
    pub fn headline(&self) -> String {
        match self {
            Activity::Offline => "Not connected".into(),
            Activity::Idle => "Idle".into(),
            Activity::Paused => "Paused".into(),
            Activity::Busy => "Busy".into(),
            Activity::Running {
                kind,
                model,
                elapsed,
                ..
            } => format!("Running {kind} \u{00b7} {model} \u{00b7} {elapsed}"),
        }
    }

    /// The line under the headline.
    pub fn detail(&self) -> String {
        match self {
            Activity::Offline => "waiting for the worker daemon".into(),
            Activity::Idle => "waiting for work".into(),
            Activity::Paused => "not claiming studio jobs".into(),
            Activity::Busy => "a job is starting".into(),
            Activity::Running { more: 0, .. } => "one job running".into(),
            Activity::Running { more, .. } => format!("+{more} more running"),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            Activity::Offline => Tone::Bad,
            Activity::Idle => Tone::Good,
            Activity::Paused => Tone::Neutral,
            Activity::Busy | Activity::Running { .. } => Tone::Busy,
        }
    }

    /// Running work glows.
    pub fn glows(&self) -> bool {
        matches!(self, Activity::Busy | Activity::Running { .. })
    }
}

/// A short labelled state with its tone and a longer hover text.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub label: String,
    pub tone: Tone,
    pub detail: String,
}

impl Signal {
    fn new(label: &str, tone: Tone, detail: impl Into<String>) -> Self {
        Self {
            label: label.to_string(),
            tone,
            detail: detail.into(),
        }
    }
}

/// Device memory held by the models the daemon keeps loaded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuMemory {
    /// Sum of the catalogue estimates of loaded (or loading) models, GB.
    pub held_gb: f32,
    /// Device total, GB; 0 when unknown.
    pub total_gb: f32,
}

/// Held memory at or above this share of the device reads as a warning.
pub const GPU_TIGHT_FRACTION: f32 = 0.9;

impl GpuMemory {
    /// Memory the loaded models hold, from their catalogue estimates.
    pub fn from_models(models: &[ModelEntry], total_gb: f32) -> Self {
        let held_gb = models
            .iter()
            .filter(|m| holds_memory(&m.state))
            .map(|m| m.vram_gb_estimate.max(0.0))
            // A plain `sum` of nothing is -0.0, which prints as "-0".
            .fold(0.0, |a, b| a + b);
        Self { held_gb, total_gb }
    }

    /// Share of the device held, 0..=1; 0 when the total is unknown.
    pub fn fraction(&self) -> f32 {
        if self.total_gb > 0.0 {
            (self.held_gb / self.total_gb).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// `≈ 6.5 / 24 GB`, or `≈ 6.5 GB` when the total is unknown.
    pub fn label(&self) -> String {
        if self.total_gb > 0.0 {
            format!(
                "\u{2248} {} / {} GB",
                format_gb(self.held_gb),
                format_gb(self.total_gb)
            )
        } else {
            format!("\u{2248} {} GB", format_gb(self.held_gb))
        }
    }

    pub fn tone(&self) -> Tone {
        if self.fraction() >= GPU_TIGHT_FRACTION {
            Tone::Busy
        } else {
            Tone::Neutral
        }
    }
}

/// Whether a model in `state` holds device memory.
pub fn holds_memory(state: &str) -> bool {
    matches!(state, "loaded" | "loading" | "unloading")
}

/// `6.5`, `24`, `0` — one decimal, none when whole.
pub fn format_gb(gb: f32) -> String {
    // Adding 0.0 turns a negative zero into a plain one.
    let rounded = (gb * 10.0).round() / 10.0 + 0.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

/// Everything the pulse is built from.
pub struct PulseInputs<'a> {
    pub link: &'a LinkState,
    pub registered: bool,
    pub registration: &'a RegistrationState,
    pub session: &'a SessionState,
    pub busy: bool,
    pub paused: bool,
    pub active: &'a [CurrentJob],
    pub models: &'a [ModelEntry],
    pub vram_total_gb: f32,
    pub now: DateTime<Utc>,
}

impl Pulse {
    pub fn build(i: PulseInputs<'_>) -> Self {
        let connected = i.link.is_connected();
        Self {
            activity: activity(&i, connected),
            daemon: daemon_signal(i.link),
            studio: studio_signal(connected, i.registered, i.registration, i.session),
            gpu: GpuMemory::from_models(i.models, i.vram_total_gb),
            can_pause: connected,
            paused: connected && i.paused,
        }
    }
}

fn activity(i: &PulseInputs<'_>, connected: bool) -> Activity {
    if !connected {
        return Activity::Offline;
    }
    // The oldest running job leads; the rest are counted.
    if let Some(first) = i.active.iter().min_by_key(|j| j.started_at) {
        return Activity::Running {
            kind: first.kind.as_str().to_string(),
            model: first.model.clone(),
            elapsed: format_duration(i.now.signed_duration_since(first.started_at)),
            more: i.active.len() - 1,
        };
    }
    if i.busy {
        Activity::Busy
    } else if i.paused {
        Activity::Paused
    } else {
        Activity::Idle
    }
}

fn daemon_signal(link: &LinkState) -> Signal {
    let (label, tone) = match link {
        LinkState::Connected { .. } => ("Daemon connected", Tone::Good),
        LinkState::Connecting => ("Connecting", Tone::Neutral),
        LinkState::Starting { .. } => ("Daemon starting", Tone::Busy),
        LinkState::Unreachable { .. } => ("Daemon unreachable", Tone::Bad),
    };
    Signal::new(label, tone, link.summary())
}

fn studio_signal(
    connected: bool,
    registered: bool,
    registration: &RegistrationState,
    session: &SessionState,
) -> Signal {
    if !connected {
        return Signal::new(
            "Studio unknown",
            Tone::Neutral,
            "the daemon holds the studio session; it does not answer",
        );
    }
    if !registered {
        return match registration {
            RegistrationState::Pending { .. } => Signal::new(
                "Awaiting approval",
                Tone::Busy,
                "the studio operator has not approved this worker yet",
            ),
            RegistrationState::Rejected { reason } => {
                Signal::new("Registration rejected", Tone::Bad, reason.clone())
            }
            RegistrationState::Pristine | RegistrationState::Approved => Signal::new(
                "Registering",
                Tone::Neutral,
                "asking the studio for a registration slot",
            ),
        };
    }
    let detail = session.summary();
    match session {
        SessionState::Connected => Signal::new("Studio connected", Tone::Good, detail),
        SessionState::Connecting => Signal::new("Connecting to studio", Tone::Busy, detail),
        SessionState::Reconnecting { attempt } => Signal {
            label: format!("Reconnecting ({attempt})"),
            tone: Tone::Busy,
            detail,
        },
        SessionState::WaitingForApproval => Signal::new("Awaiting approval", Tone::Busy, detail),
        SessionState::AuthFailed { .. } => Signal::new("Studio auth failed", Tone::Bad, detail),
        SessionState::Fatal { .. } => Signal::new("Studio session ended", Tone::Bad, detail),
        SessionState::Stopped => Signal::new("Studio stopped", Tone::Neutral, detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_api::ModelSourceBrief;
    use crate::runtime::JobSource;
    use crate::types::{ModelEngine, TaskKind};

    fn connected() -> LinkState {
        LinkState::Connected {
            url: "http://127.0.0.1:4787".into(),
            version: "0.4.9".into(),
        }
    }

    fn job(id: &str, model: &str, secs_ago: i64, now: DateTime<Utc>) -> CurrentJob {
        CurrentJob {
            job_id: id.into(),
            kind: TaskKind::Image,
            model: model.into(),
            prompt: String::new(),
            started_at: now - chrono::Duration::seconds(secs_ago),
            source: JobSource::Local,
        }
    }

    fn model(state: &str, gb: f32) -> ModelEntry {
        ModelEntry {
            id: format!("m-{state}"),
            display_name: "M".into(),
            kind: TaskKind::Llm,
            vram_gb_estimate: gb,
            source: ModelSourceBrief {
                engine: ModelEngine::LlamaCpp,
            },
            enabled: true,
            exclusive_group: None,
            state: state.into(),
            resident: false,
            since: None,
            error: None,
            loadable: true,
        }
    }

    struct Given {
        link: LinkState,
        registered: bool,
        registration: RegistrationState,
        session: SessionState,
        busy: bool,
        paused: bool,
        active: Vec<CurrentJob>,
        models: Vec<ModelEntry>,
        now: DateTime<Utc>,
    }

    impl Default for Given {
        fn default() -> Self {
            Self {
                link: connected(),
                registered: true,
                registration: RegistrationState::Approved,
                session: SessionState::Connected,
                busy: false,
                paused: false,
                active: Vec::new(),
                models: Vec::new(),
                now: Utc::now(),
            }
        }
    }

    fn pulse(g: &Given) -> Pulse {
        Pulse::build(PulseInputs {
            link: &g.link,
            registered: g.registered,
            registration: &g.registration,
            session: &g.session,
            busy: g.busy,
            paused: g.paused,
            active: &g.active,
            models: &g.models,
            vram_total_gb: 24.0,
            now: g.now,
        })
    }

    #[test]
    fn an_idle_connected_worker_reads_calm() {
        let p = pulse(&Given::default());
        assert_eq!(p.activity, Activity::Idle);
        assert_eq!(p.activity.headline(), "Idle");
        assert_eq!(p.activity.tone(), Tone::Good);
        assert!(!p.activity.glows());
        assert_eq!(p.daemon.label, "Daemon connected");
        assert_eq!(p.studio.label, "Studio connected");
        assert!(p.can_pause && !p.paused);
    }

    #[test]
    fn the_oldest_running_job_leads_and_the_rest_are_counted() {
        let now = Utc::now();
        let g = Given {
            active: vec![job("b", "young", 3, now), job("a", "sdxl", 72, now)],
            now,
            ..Given::default()
        };
        let p = pulse(&g);
        assert_eq!(
            p.activity.headline(),
            "Running image \u{00b7} sdxl \u{00b7} 1m 12s"
        );
        assert_eq!(p.activity.detail(), "+1 more running");
        assert!(p.activity.glows());

        let g = Given {
            active: vec![job("a", "sdxl", 5, now)],
            now,
            ..Given::default()
        };
        assert_eq!(pulse(&g).activity.detail(), "one job running");
    }

    #[test]
    fn paused_busy_and_offline_each_say_so() {
        let p = pulse(&Given {
            paused: true,
            ..Given::default()
        });
        assert_eq!(p.activity, Activity::Paused);
        assert!(p.paused);
        assert_eq!(p.activity.detail(), "not claiming studio jobs");

        let p = pulse(&Given {
            busy: true,
            ..Given::default()
        });
        assert_eq!(p.activity, Activity::Busy);
        assert!(p.activity.glows());

        let p = pulse(&Given {
            link: LinkState::Unreachable {
                error: "e".into(),
                started_daemon: true,
            },
            paused: true,
            ..Given::default()
        });
        assert_eq!(p.activity, Activity::Offline);
        assert_eq!(p.activity.tone(), Tone::Bad);
        assert_eq!(p.daemon.label, "Daemon unreachable");
        assert_eq!(p.studio.label, "Studio unknown");
        assert!(!p.can_pause && !p.paused, "no stale pause state");
    }

    #[test]
    fn every_link_state_has_its_signal() {
        let cases = [
            (LinkState::Connecting, "Connecting", Tone::Neutral),
            (
                LinkState::Starting { error: "e".into() },
                "Daemon starting",
                Tone::Busy,
            ),
        ];
        for (link, label, tone) in cases {
            let s = pulse(&Given {
                link,
                ..Given::default()
            })
            .daemon;
            assert_eq!((s.label.as_str(), s.tone), (label, tone));
        }
    }

    #[test]
    fn the_studio_signal_follows_registration_then_the_session() {
        let unregistered = |registration| Given {
            registered: false,
            registration,
            ..Given::default()
        };
        let label = |g: &Given| pulse(g).studio.label;
        assert_eq!(
            label(&unregistered(RegistrationState::Pending {
                request_id: "r".into(),
                since: Utc::now()
            })),
            "Awaiting approval"
        );
        let rejected = pulse(&unregistered(RegistrationState::Rejected {
            reason: "unknown contributor".into(),
        }))
        .studio;
        assert_eq!(rejected.label, "Registration rejected");
        assert_eq!(rejected.tone, Tone::Bad);
        assert_eq!(rejected.detail, "unknown contributor");
        assert_eq!(
            label(&unregistered(RegistrationState::Pristine)),
            "Registering"
        );

        let with_session = |session| Given {
            session,
            ..Given::default()
        };
        for (session, expected, tone) in [
            (SessionState::Connecting, "Connecting to studio", Tone::Busy),
            (
                SessionState::Reconnecting { attempt: 4 },
                "Reconnecting (4)",
                Tone::Busy,
            ),
            (
                SessionState::WaitingForApproval,
                "Awaiting approval",
                Tone::Busy,
            ),
            (
                SessionState::AuthFailed { reason: "x".into() },
                "Studio auth failed",
                Tone::Bad,
            ),
            (
                SessionState::Fatal { reason: "x".into() },
                "Studio session ended",
                Tone::Bad,
            ),
            (SessionState::Stopped, "Studio stopped", Tone::Neutral),
        ] {
            let s = pulse(&with_session(session)).studio;
            assert_eq!((s.label.as_str(), s.tone), (expected, tone));
        }
    }

    #[test]
    fn gpu_memory_counts_the_models_that_hold_it() {
        let g = Given {
            models: vec![
                model("loaded", 6.0),
                model("loading", 0.5),
                model("unloading", 1.0),
                model("unloaded", 12.0),
                model("failed", 3.0),
            ],
            ..Given::default()
        };
        let gpu = pulse(&g).gpu;
        assert_eq!(gpu.held_gb, 7.5);
        assert_eq!(gpu.label(), "\u{2248} 7.5 / 24 GB");
        assert!((gpu.fraction() - 7.5 / 24.0).abs() < 1e-6);
        assert_eq!(gpu.tone(), Tone::Neutral);
    }

    #[test]
    fn gpu_memory_warns_when_tight_and_copes_without_a_total() {
        let tight = GpuMemory {
            held_gb: 22.0,
            total_gb: 24.0,
        };
        assert_eq!(tight.tone(), Tone::Busy);
        let unknown = GpuMemory {
            held_gb: 2.0,
            total_gb: 0.0,
        };
        assert_eq!(unknown.fraction(), 0.0);
        assert_eq!(unknown.label(), "\u{2248} 2 GB");
        let over = GpuMemory {
            held_gb: 30.0,
            total_gb: 24.0,
        };
        assert_eq!(over.fraction(), 1.0);
    }

    #[test]
    fn no_loaded_models_hold_plain_zero() {
        let gpu = GpuMemory::from_models(&[], 24.0);
        assert_eq!(gpu.label(), "\u{2248} 0 / 24 GB");
    }

    #[test]
    fn gigabytes_read_short() {
        assert_eq!(format_gb(24.0), "24");
        assert_eq!(format_gb(6.54), "6.5");
        assert_eq!(format_gb(0.0), "0");
        assert_eq!(format_gb(0.04), "0");
        assert_eq!(format_gb(-0.0), "0");
        assert_eq!(format_gb(-0.04), "0");
    }
}
