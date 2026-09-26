//! Worker: the worker's state, registration, studio link, hardware, local
//! API and version on one page (formerly the Status and About tabs).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eframe::egui::{self, Align, Layout, RichText};
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::auto_register::RegistrationState;
use crate::config::Config;
use crate::runtime::{self, GpuRuntimeStatus, HeartbeatOutcome, HeartbeatStatus, SessionState};
use crate::{update, AGENT_VERSION, RELEASE_NAME};

use super::super::format::{format_age, format_duration};
use super::super::icons::{self, Icon};
use super::super::pulse::{format_gb, Activity};
use super::super::theme::{Palette, Tone};
use super::super::widgets;

/// Tracing target of the manual update check.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::ui::about=info`.
const TRACE_TARGET: &str = "studio_worker::ui::about";

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// Where the worker is with the studio's approval.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistrationView {
    /// Asking for a registration slot; transient on first launch.
    Initialising,
    /// Waiting for the studio operator; the request id helps them find it.
    Pending {
        request_id: String,
        since: DateTime<Utc>,
    },
    /// The operator said no; a reset asks again.
    Rejected {
        reason: String,
    },
    Registered {
        worker_id: String,
    },
}

impl RegistrationView {
    /// Registered wins whatever the registration state says.
    pub fn build(cfg: &Config, registered: bool, registration: &RegistrationState) -> Self {
        if registered {
            return Self::Registered {
                worker_id: cfg.worker_id.clone().unwrap_or_default(),
            };
        }
        match registration {
            RegistrationState::Pending { request_id, since } => Self::Pending {
                request_id: request_id.clone(),
                since: *since,
            },
            RegistrationState::Rejected { reason } => Self::Rejected {
                reason: reason.clone(),
            },
            RegistrationState::Pristine | RegistrationState::Approved => Self::Initialising,
        }
    }
}

/// The last heartbeat, as the page says it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatSummary {
    pub when: DateTime<Utc>,
    pub ok: bool,
    pub reason: Option<String>,
}

impl HeartbeatSummary {
    pub fn from(status: &HeartbeatStatus) -> Self {
        let (ok, reason) = match &status.outcome {
            HeartbeatOutcome::Ok => (true, None),
            HeartbeatOutcome::Err { reason } => (false, Some(reason.clone())),
        };
        Self {
            when: status.last_attempt_at,
            ok,
            reason,
        }
    }

    /// `ok · 5s ago` or `error · 5s ago · <reason>`.
    pub fn line(&self, now: DateTime<Utc>) -> (String, Tone) {
        let when = format_age(now, self.when);
        if self.ok {
            (format!("ok \u{00b7} {when}"), Tone::Good)
        } else {
            let reason = self.reason.as_deref().unwrap_or("unknown");
            (
                format!("error \u{00b7} {when} \u{00b7} {reason}"),
                Tone::Bad,
            )
        }
    }
}

/// The tone of the studio session's state.
pub fn session_tone(session: &SessionState) -> Tone {
    match session {
        SessionState::Connected => Tone::Good,
        SessionState::Connecting
        | SessionState::Reconnecting { .. }
        | SessionState::WaitingForApproval => Tone::Busy,
        SessionState::AuthFailed { .. } | SessionState::Fatal { .. } => Tone::Bad,
        SessionState::Stopped => Tone::Neutral,
    }
}

/// The daemon's side of the page.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerFacts {
    pub registration: RegistrationView,
    pub api_base_url: String,
    pub session: String,
    pub session_tone: Tone,
    pub heartbeat: Option<HeartbeatSummary>,
    /// GPU runtime readiness: `None` until probed.
    pub gpu: Option<(bool, String)>,
    pub vram_total_gb: f32,
    pub vram_threshold_gb: f32,
    pub held_gb: f32,
    pub local_api_url: Option<String>,
}

/// Everything [`WorkerFacts`] is built from.
pub struct FactsInputs<'a> {
    pub cfg: &'a Config,
    pub registered: bool,
    pub registration: &'a RegistrationState,
    pub session: &'a SessionState,
    pub heartbeat: Option<&'a HeartbeatStatus>,
    pub gpu: Option<&'a GpuRuntimeStatus>,
    pub vram_total_gb: f32,
    pub held_gb: f32,
    pub local_api_url: Option<String>,
}

impl WorkerFacts {
    pub fn build(i: FactsInputs<'_>) -> Self {
        Self {
            registration: RegistrationView::build(i.cfg, i.registered, i.registration),
            api_base_url: i.cfg.api_base_url.clone(),
            session: i.session.summary(),
            session_tone: session_tone(i.session),
            heartbeat: i.heartbeat.map(HeartbeatSummary::from),
            gpu: i.gpu.map(|g| (g.ok, g.detail.clone())),
            vram_total_gb: i.vram_total_gb,
            vram_threshold_gb: i.cfg.vram_threshold_gb,
            held_gb: i.held_gb,
            local_api_url: i.local_api_url,
        }
    }
}

/// The last manual update check.
#[derive(Debug, Clone, Default)]
pub struct AboutState {
    pub last_check: Arc<Mutex<Option<CheckLine>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckLine {
    InFlight,
    Result(String),
}

/// Versions and where things live; shown whether or not the daemon answers.
#[derive(Debug, Clone, PartialEq)]
pub struct AboutView {
    pub version: &'static str,
    /// The daemon's version, when it answers.  Differs from `version`
    /// after the daemon updated itself and the tray UI has not restarted.
    pub daemon_version: Option<String>,
    pub release_name: &'static str,
    pub config_path: PathBuf,
    pub last_check: Option<CheckLine>,
}

impl AboutView {
    pub fn build(state: &AboutState, config_path: &Path, daemon_version: Option<String>) -> Self {
        Self {
            version: AGENT_VERSION,
            daemon_version,
            release_name: RELEASE_NAME,
            config_path: config_path.to_path_buf(),
            last_check: state.last_check.lock().clone(),
        }
    }

    /// The daemon's version line and its tone.
    pub fn daemon_line(&self) -> (String, Tone) {
        match &self.daemon_version {
            Some(v) if v == self.version => (v.clone(), Tone::Neutral),
            Some(v) => (format!("{v} (restart the tray UI to match)"), Tone::Busy),
            None => ("not reachable".into(), Tone::Bad),
        }
    }
}

/// The release feed the daemon is configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFeed {
    pub url: String,
    pub prerelease: bool,
}

// ---------------------------------------------------------------------------
// Update check
// ---------------------------------------------------------------------------

fn spawn_check(tokio: Handle, slot: Arc<Mutex<Option<CheckLine>>>, feed: UpdateFeed) {
    *slot.lock() = Some(CheckLine::InFlight);
    tokio.spawn(async move {
        let outcome = run_check(feed).await;
        let line = record_check_outcome(outcome);
        *slot.lock() = Some(CheckLine::Result(line));
    });
}

/// Log the outcome of a manual "Check for updates" and return the line to
/// show; the same wording as `studio-worker check-update`.
fn record_check_outcome(outcome: anyhow::Result<update::CheckOutcome>) -> String {
    match outcome {
        Ok(o) => {
            match &o {
                update::CheckOutcome::UpToDate { current } => tracing::info!(
                    target: TRACE_TARGET,
                    op = "manual_check",
                    result = "up_to_date",
                    current = %current,
                    "manual update check completed"
                ),
                update::CheckOutcome::NewerAvailable { current, latest } => tracing::info!(
                    target: TRACE_TARGET,
                    op = "manual_check",
                    result = "newer_available",
                    current = %current,
                    latest = %latest,
                    "manual update check found a newer release"
                ),
            }
            runtime::format_check_outcome(&o)
        }
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "manual_check",
                error = %e,
                "manual update check failed"
            );
            format!("check failed: {e}")
        }
    }
}

async fn run_check(feed: UpdateFeed) -> anyhow::Result<update::CheckOutcome> {
    let current = semver::Version::parse(AGENT_VERSION)?;
    let outcome =
        tokio::task::spawn_blocking(move || update::check(&feed.url, &current, feed.prerelease))
            .await??;
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerAction {
    /// Pause (`true`) or resume claiming studio jobs.
    SetPaused(bool),
    /// Clear a rejected registration and ask the studio again.
    ResetRegistration,
}

/// Why the daemon's side of the pages is missing.
#[derive(Debug, Clone, PartialEq)]
pub struct Offline {
    /// What the UI is doing about it.
    pub summary: String,
    /// The last error, if any.
    pub detail: String,
    /// Where a daemon the UI starts writes its output.
    pub daemon_log: PathBuf,
}

/// What the page needs besides its view.
pub struct WorkerContext<'a> {
    pub activity: &'a Activity,
    pub paused: bool,
    /// `None` while the daemon does not answer.
    pub facts: Option<&'a WorkerFacts>,
    /// Set while the daemon does not answer.
    pub offline: Option<&'a Offline>,
    pub about: &'a AboutView,
    pub about_state: &'a AboutState,
    pub tokio: &'a Handle,
    pub feed: &'a UpdateFeed,
    pub glow: f32,
}

/// Width below which the cards stack in one column, in points.
const TWO_COLUMNS_FROM: f32 = 760.0;

/// Draw the page (below the "daemon not reachable" card while the daemon
/// does not answer); answers the operator's action.
pub fn render(ui: &mut egui::Ui, cx: WorkerContext<'_>) -> Option<WorkerAction> {
    widgets::page_title(
        ui,
        "Worker",
        "This machine's worker: its state, its place in the studio, its hardware and version.",
    );
    let mut action = None;
    if let Some(offline) = cx.offline {
        unreachable_card(ui, offline);
        ui.add_space(12.0);
    }
    if let Some(facts) = cx.facts {
        action = hero(ui, cx.activity, cx.paused, cx.glow);
        ui.add_space(12.0);
        if let Some(a) = registration_card(ui, facts) {
            action = Some(a);
        }
        ui.add_space(12.0);
        two_columns(
            ui,
            |ui| studio_card(ui, facts),
            |ui| hardware_card(ui, facts),
        );
        ui.add_space(12.0);
    }
    two_columns(
        ui,
        |ui| about_card(ui, cx.about, cx.about_state, cx.tokio, cx.feed),
        |ui| local_api_card(ui, cx.facts.and_then(|f| f.local_api_url.as_deref())),
    );
    action
}

/// Two cards side by side, as tall as each other, or stacked on a narrow
/// window.
fn two_columns(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    if ui.available_width() < TWO_COLUMNS_FROM {
        widgets::card(ui, left);
        ui.add_space(12.0);
        widgets::card(ui, right);
        return;
    }
    widgets::card_pair(ui, left, right);
}

fn hero(ui: &mut egui::Ui, activity: &Activity, paused: bool, glow: f32) -> Option<WorkerAction> {
    let p = Palette::of_ui(ui);
    let mut action = None;
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            let halo = if activity.glows() { glow } else { 0.0 };
            widgets::status_dot(ui, activity.tone(), halo);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(activity.headline())
                        .size(20.0)
                        .strong()
                        .color(p.text),
                );
                ui.label(widgets::muted(ui, activity.detail()));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, hint) = if paused {
                    ("Resume", "start accepting studio job offers again")
                } else {
                    (
                        "Pause",
                        "stop accepting studio job offers; a running job finishes",
                    )
                };
                if widgets::primary_button(ui, label, true, 110.0)
                    .on_hover_text(hint)
                    .clicked()
                {
                    action = Some(WorkerAction::SetPaused(!paused));
                }
            });
        });
    });
    action
}

fn registration_card(ui: &mut egui::Ui, facts: &WorkerFacts) -> Option<WorkerAction> {
    let p = Palette::of_ui(ui);
    let mut action = None;
    widgets::card(ui, |ui| {
        widgets::section_label(ui, "REGISTRATION");
        match &facts.registration {
            RegistrationView::Registered { worker_id } => {
                ui.horizontal(|ui| {
                    widgets::pill(ui, "Approved", Tone::Good);
                    ui.label(widgets::muted(ui, "Worker id"));
                    ui.label(RichText::new(worker_id).monospace().color(p.text));
                    widgets::copy_button(ui, "worker-id", "Copy", worker_id);
                });
            }
            RegistrationView::Initialising => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(format!(
                            "Asking {} for a registration slot\u{2026}",
                            facts.api_base_url
                        ))
                        .color(p.text),
                    );
                });
                ui.label(widgets::muted(
                    ui,
                    "No action needed: the worker keeps retrying until it gets through.",
                ));
            }
            RegistrationView::Pending { request_id, since } => {
                ui.horizontal(|ui| {
                    widgets::pill(ui, "Waiting for approval", Tone::Busy);
                    ui.label(widgets::muted(
                        ui,
                        format!("waiting {}", format_duration(Utc::now() - *since)),
                    ));
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "This worker asked {} to join and waits for the studio operator to \
                         approve it.  It keeps polling in the background.",
                        facts.api_base_url
                    ))
                    .color(p.text),
                );
                ui.horizontal(|ui| {
                    ui.label(widgets::muted(ui, "Request id"));
                    ui.label(RichText::new(request_id).monospace().color(p.text));
                    widgets::copy_button(ui, "request-id", "Copy", request_id);
                });
                ui.label(widgets::muted(
                    ui,
                    "Share the request id with the studio operator so they find this worker quickly.",
                ));
            }
            RegistrationView::Rejected { reason } => {
                widgets::pill(ui, "Registration rejected", Tone::Bad);
                ui.add_space(4.0);
                let text = if reason.is_empty() {
                    "The studio operator rejected this worker's registration.".to_string()
                } else {
                    format!("The studio operator rejected this worker's registration: {reason}")
                };
                widgets::problem_box(ui, &text);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "Local models and the local API keep working.  To ask {} again, check \
                         with its operator why, then reset the registration.",
                        facts.api_base_url
                    ))
                    .color(p.text),
                );
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "Reset registration", true, 160.0)
                        .on_hover_text("clear the local registration state and ask again")
                        .clicked()
                    {
                        action = Some(WorkerAction::ResetRegistration);
                    }
                    ui.label(widgets::muted(
                        ui,
                        "Same as `studio-worker register --reset`, without a restart.",
                    ));
                });
            }
        }
    });
    action
}

fn studio_card(ui: &mut egui::Ui, facts: &WorkerFacts) {
    {
        widgets::section_label(ui, "STUDIO");
        widgets::facts(ui, "studio-facts", |rows| {
            rows.toned("Connection", &facts.session, facts.session_tone);
            match &facts.heartbeat {
                None => rows.text("Last heartbeat", "never"),
                Some(h) => {
                    let (line, tone) = h.line(Utc::now());
                    rows.toned("Last heartbeat", line, tone);
                }
            }
            rows.mono("API base URL", &facts.api_base_url);
        });
    }
}

fn hardware_card(ui: &mut egui::Ui, facts: &WorkerFacts) {
    {
        widgets::section_label(ui, "HARDWARE");
        widgets::facts(ui, "hardware-facts", |rows| {
            match &facts.gpu {
                Some((true, detail)) => rows.toned("GPU runtime", detail, Tone::Good),
                Some((false, detail)) => rows.toned("GPU runtime", detail, Tone::Bad),
                None => rows.text("GPU runtime", "not probed yet"),
            }
            rows.text(
                "VRAM total",
                format!("{} GB", format_gb(facts.vram_total_gb)),
            );
            rows.text(
                "VRAM threshold",
                format!("{} GB per claim", format_gb(facts.vram_threshold_gb)),
            );
            rows.text(
                "Loaded models",
                format!("\u{2248} {} GB held", format_gb(facts.held_gb)),
            );
        });
    }
}

fn local_api_card(ui: &mut egui::Ui, url: Option<&str>) {
    {
        widgets::section_label(ui, "LOCAL API");
        match url {
            Some(url) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(url).monospace());
                    widgets::copy_button(ui, "local-api", "Copy", url);
                });
                ui.label(widgets::muted(
                    ui,
                    "Generate on this machine without the studio; its jobs show under Local.",
                ));
            }
            None => {
                ui.label(widgets::muted(
                    ui,
                    "Not bound (the daemon does not answer yet).",
                ));
            }
        }
    }
}

fn about_card(
    ui: &mut egui::Ui,
    view: &AboutView,
    state: &AboutState,
    tokio: &Handle,
    feed: &UpdateFeed,
) {
    let p = Palette::of_ui(ui);
    {
        widgets::section_label(ui, "ABOUT");
        widgets::facts(ui, "about-facts", |rows| {
            rows.mono("Tray UI", view.version);
            let (line, tone) = view.daemon_line();
            rows.mono_toned("Daemon", &line, tone);
            rows.mono("Sentry release", view.release_name);
            let path = view.config_path.to_string_lossy().to_string();
            rows.row("Config file", |ui| {
                ui.label(RichText::new(&path).monospace().color(p.text));
                widgets::copy_button(ui, "config-path", "Copy", &path);
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let busy = matches!(view.last_check, Some(CheckLine::InFlight));
            if widgets::button(ui, "Check for updates", !busy, 150.0).clicked() {
                spawn_check(tokio.clone(), state.last_check.clone(), feed.clone());
            }
            match &view.last_check {
                None => {}
                Some(CheckLine::InFlight) => {
                    ui.spinner();
                    ui.label(widgets::muted(ui, "Checking the release feed\u{2026}"));
                }
                Some(CheckLine::Result(line)) => {
                    let tone = if line.starts_with("check failed") {
                        Tone::Bad
                    } else {
                        Tone::Neutral
                    };
                    ui.label(RichText::new(line).color(p.tone(tone)));
                }
            }
        });
    }
}

/// The card that stands in for the daemon's side of the pages while it
/// does not answer.
pub fn unreachable_card(ui: &mut egui::Ui, offline: &Offline) {
    let (summary, detail, daemon_log) = (&offline.summary, &offline.detail, &offline.daemon_log);
    let p = Palette::of_ui(ui);
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            icons::show(ui, Icon::Worker, 26.0, p.bad);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Worker daemon not reachable")
                        .size(18.0)
                        .strong()
                        .color(p.text),
                );
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(summary.as_str()).color(p.muted));
                });
            });
        });
        if !detail.is_empty() {
            ui.add_space(8.0);
            widgets::tinted_box(ui, Tone::Neutral, detail.as_str());
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "The tray UI shows what the daemon (`studio-worker run`) does.  It starts one \
                 when none is running and keeps retrying; nothing here is stale.",
            )
            .color(p.text),
        );
        let log = daemon_log.to_string_lossy().to_string();
        ui.horizontal_wrapped(|ui| {
            ui.label(widgets::muted(ui, "A daemon it starts writes to"));
            ui.label(RichText::new(&log).monospace().color(p.text));
            widgets::copy_button(ui, "daemon-log", "Copy", &log);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn registered_cfg() -> Config {
        Config {
            worker_id: Some("w-abc".into()),
            auth_token: Some("tok-xyz".into()),
            api_base_url: "https://studio.example".into(),
            vram_threshold_gb: 12.0,
            ..Config::default()
        }
    }

    #[test]
    fn registration_reads_initialising_pending_rejected_or_registered() {
        let cfg = Config::default();
        assert_eq!(
            RegistrationView::build(&cfg, false, &RegistrationState::Pristine),
            RegistrationView::Initialising
        );
        let since = Utc::now();
        assert_eq!(
            RegistrationView::build(
                &cfg,
                false,
                &RegistrationState::Pending {
                    request_id: "rr-42".into(),
                    since
                }
            ),
            RegistrationView::Pending {
                request_id: "rr-42".into(),
                since
            }
        );
        assert_eq!(
            RegistrationView::build(
                &cfg,
                false,
                &RegistrationState::Rejected {
                    reason: "unknown contributor".into()
                }
            ),
            RegistrationView::Rejected {
                reason: "unknown contributor".into()
            }
        );
    }

    #[test]
    fn being_registered_wins_over_a_stale_registration_state() {
        let view = RegistrationView::build(
            &registered_cfg(),
            true,
            &RegistrationState::Pending {
                request_id: "rr-stale".into(),
                since: Utc::now(),
            },
        );
        assert_eq!(
            view,
            RegistrationView::Registered {
                worker_id: "w-abc".into()
            }
        );
    }

    #[test]
    fn a_heartbeat_reads_ok_or_its_error() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 30).unwrap();
        let then = Utc.with_ymd_and_hms(2026, 5, 25, 12, 0, 18).unwrap();
        let ok = HeartbeatSummary::from(&HeartbeatStatus {
            last_attempt_at: then,
            outcome: HeartbeatOutcome::Ok,
        });
        assert_eq!(ok.line(now), ("ok \u{00b7} 12s ago".into(), Tone::Good));
        let err = HeartbeatSummary::from(&HeartbeatStatus {
            last_attempt_at: then,
            outcome: HeartbeatOutcome::Err {
                reason: "5xx".into(),
            },
        });
        assert_eq!(
            err.line(now),
            ("error \u{00b7} 12s ago \u{00b7} 5xx".into(), Tone::Bad)
        );
    }

    #[test]
    fn session_states_have_tones() {
        assert_eq!(session_tone(&SessionState::Connected), Tone::Good);
        assert_eq!(
            session_tone(&SessionState::Reconnecting { attempt: 2 }),
            Tone::Busy
        );
        assert_eq!(
            session_tone(&SessionState::AuthFailed { reason: "x".into() }),
            Tone::Bad
        );
        assert_eq!(session_tone(&SessionState::Stopped), Tone::Neutral);
    }

    fn facts(registered: bool, registration: RegistrationState) -> WorkerFacts {
        let hb = HeartbeatStatus {
            last_attempt_at: Utc::now(),
            outcome: HeartbeatOutcome::Ok,
        };
        let gpu = GpuRuntimeStatus {
            ok: false,
            detail: "install libvulkan1".into(),
        };
        WorkerFacts::build(FactsInputs {
            cfg: &registered_cfg(),
            registered,
            registration: &registration,
            session: &SessionState::Reconnecting { attempt: 4 },
            heartbeat: Some(&hb),
            gpu: Some(&gpu),
            vram_total_gb: 24.0,
            held_gb: 1.5,
            local_api_url: Some("http://127.0.0.1:4787".into()),
        })
    }

    #[test]
    fn facts_carry_every_signal_the_page_shows() {
        let f = facts(true, RegistrationState::Approved);
        assert_eq!(f.api_base_url, "https://studio.example");
        assert_eq!(f.session, "reconnecting (attempt 4)\u{2026}");
        assert_eq!(f.session_tone, Tone::Busy);
        assert!(f.heartbeat.as_ref().unwrap().ok);
        assert_eq!(f.gpu, Some((false, "install libvulkan1".into())));
        assert_eq!(f.vram_threshold_gb, 12.0);
        assert_eq!(f.held_gb, 1.5);
    }

    #[test]
    fn about_says_when_the_daemon_runs_another_version() {
        let state = AboutState::default();
        let same = AboutView::build(&state, Path::new("/tmp/c.toml"), Some(AGENT_VERSION.into()));
        assert_eq!(same.daemon_line().1, Tone::Neutral);
        let other = AboutView::build(&state, Path::new("/tmp/c.toml"), Some("0.0.1".into()));
        assert!(other.daemon_line().0.contains("restart the tray UI"));
        assert_eq!(other.daemon_line().1, Tone::Busy);
        let none = AboutView::build(&state, Path::new("/tmp/c.toml"), None);
        assert_eq!(none.daemon_line(), ("not reachable".into(), Tone::Bad));
        assert_eq!(none.release_name, RELEASE_NAME);
        assert_eq!(none.config_path, PathBuf::from("/tmp/c.toml"));
        assert!(none.last_check.is_none());

        *state.last_check.lock() = Some(CheckLine::Result("up to date".into()));
        let view = AboutView::build(&state, Path::new("/tmp/c.toml"), None);
        assert_eq!(
            view.last_check,
            Some(CheckLine::Result("up to date".into()))
        );
    }

    use crate::test_support::capture;
    use semver::Version;

    #[test]
    fn a_manual_check_logs_up_to_date_at_info() {
        let logs = capture(|| {
            let line = record_check_outcome(Ok(update::CheckOutcome::UpToDate {
                current: Version::new(1, 2, 3),
            }));
            assert_eq!(line, "up to date: 1.2.3");
        });
        assert!(logs.contains("INFO"), "{logs}");
        assert!(logs.contains("studio_worker::ui::about"), "{logs}");
        assert!(logs.contains("op=\"manual_check\""), "{logs}");
        assert!(logs.contains("result=\"up_to_date\""), "{logs}");
    }

    #[test]
    fn a_manual_check_logs_a_newer_release() {
        let logs = capture(|| {
            let line = record_check_outcome(Ok(update::CheckOutcome::NewerAvailable {
                current: Version::new(1, 0, 0),
                latest: Version::new(2, 0, 0),
            }));
            assert_eq!(line, "update available: 1.0.0 -> 2.0.0");
        });
        assert!(logs.contains("result=\"newer_available\""), "{logs}");
        assert!(logs.contains("2.0.0"), "{logs}");
    }

    #[test]
    fn a_failed_manual_check_logs_at_warn() {
        let logs = capture(|| {
            let line = record_check_outcome(Err(anyhow::anyhow!("feed exploded")));
            assert!(line.contains("check failed") && line.contains("feed exploded"));
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("op=\"manual_check\""), "{logs}");
        assert!(logs.contains("feed exploded"), "{logs}");
    }

    fn tokio_handle() -> Handle {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
        })
        .handle()
        .clone()
    }

    #[test]
    fn every_registration_state_draws_connected_and_not() {
        let state = AboutState::default();
        let feed = UpdateFeed {
            url: "http://127.0.0.1:9".into(),
            prerelease: false,
        };
        let tokio = tokio_handle();
        let all = [
            facts(true, RegistrationState::Approved),
            facts(false, RegistrationState::Pristine),
            facts(
                false,
                RegistrationState::Pending {
                    request_id: "rr".into(),
                    since: Utc::now(),
                },
            ),
            facts(
                false,
                RegistrationState::Rejected {
                    reason: String::new(),
                },
            ),
        ];
        for check in [
            None,
            Some(CheckLine::InFlight),
            Some(CheckLine::Result("check failed: x".into())),
        ] {
            *state.last_check.lock() = check;
            let about = AboutView::build(&state, Path::new("/tmp/c.toml"), Some("0.0.1".into()));
            let offline = Offline {
                summary: "retrying".into(),
                detail: "no discovery file".into(),
                daemon_log: PathBuf::from("/tmp/daemon.log"),
            };
            for f in all.iter().map(Some).chain([None]) {
                for (activity, paused) in [
                    (Activity::Idle, false),
                    (Activity::Paused, true),
                    (Activity::Busy, false),
                ] {
                    egui::__run_test_ui(|ui| {
                        let action = render(
                            ui,
                            WorkerContext {
                                activity: &activity,
                                paused,
                                facts: f,
                                offline: f.is_none().then_some(&offline),
                                about: &about,
                                about_state: &state,
                                tokio: &tokio,
                                feed: &feed,
                                glow: 0.5,
                            },
                        );
                        assert_eq!(action, None);
                    });
                }
            }
        }
    }
}
