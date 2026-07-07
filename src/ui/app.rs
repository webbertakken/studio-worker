//! The eframe `App` impl.  Holds shared state (the same `Arc<Mutex<…>>`
//! handles the runtime loops use) and dispatches to per-tab renderers.

use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::{
    config::SharedConfig,
    runtime::{WorkerObservers, HEARTBEAT_INTERVAL},
    sys,
    types::LogEntry,
};

use super::{
    notifier::{decide, NotificationPrefs, Notifier, NotifyDecision},
    tab::Tab,
    tabs::{
        about::{self as about_tab, AboutState},
        config::{self as config_tab, ConfigDraft},
        jobs as jobs_tab,
        logs::{self as logs_tab, LogFilter},
        status as status_tab,
    },
    tray::{self, TrayVariant},
};

/// Tracing target for App-level lifecycle + tray events.  Stable so
/// operators can filter with `RUST_LOG=studio_worker::ui::app=info`.
const TRACE_TARGET: &str = "studio_worker::ui::app";

/// Emit a structured breadcrumb when the tray health indicator flips
/// between idle / busy / disconnected.  Pulled out of
/// [`App::refresh_tray_variant`] so it is unit-testable without
/// constructing a (non-`Send`) `App` + a real OS tray.
fn log_tray_variant_change(from: TrayVariant, to: TrayVariant) {
    tracing::info!(
        target: TRACE_TARGET,
        op = "tray_variant",
        from = ?from,
        to = ?to,
        "tray status indicator changed"
    );
}

/// Everything `App` needs to render and act on the world.
pub struct AppDeps {
    pub cfg: SharedConfig,
    pub logs: Arc<Mutex<Vec<LogEntry>>>,
    pub busy: Arc<AtomicBool>,
    /// Runtime-only Pause / Resume flag, shared with the WS session
    /// (heartbeat advertises `auto_enabled = !paused`, new offers
    /// are rejected while set).  Not persisted to `Config`.
    pub paused: Arc<AtomicBool>,
    pub observers: WorkerObservers,
    pub stop: Arc<AtomicBool>,
    pub config_path: PathBuf,
    pub tokio: Handle,
}

pub struct App {
    deps: AppDeps,
    tab: Tab,
    registration: crate::auto_register::SharedRegistration,
    config_draft: ConfigDraft,
    log_filter: LogFilter,
    about_state: AboutState,
    vram_total_gb: f32,
    /// Identity (`job_id` + `finished_at`) of the newest recent-job we
    /// have already raised a notification for.  Tracking identity
    /// rather than ring length means a saturated, capped
    /// `recent_jobs` ring (whose length pins at `RECENT_JOBS_CAP`)
    /// can't make new arrivals invisible.
    last_notified: Option<(String, chrono::DateTime<chrono::Utc>)>,
    notifier: Box<dyn Notifier + Send + Sync>,
    notification_prefs: NotificationPrefs,
    tray_variant: TrayVariant,
    quit_requested: Arc<std::sync::atomic::AtomicBool>,
    tray: Option<super::tray_host::TrayHandle>,
    /// One-shot request to minimise the window on the first frame
    /// (config `start_minimised`, default true).  Minimised to the
    /// taskbar — not hidden — so the window stays reachable even when
    /// no tray host is available.
    start_minimised_pending: bool,
}

impl App {
    pub fn new(deps: AppDeps) -> Self {
        Self::with_notifier(deps, Self::default_notifier())
    }

    /// Used by tests to inject a `CapturingNotifier`.
    pub fn with_notifier(deps: AppDeps, notifier: Box<dyn Notifier + Send + Sync>) -> Self {
        Self::with_notifier_and_registration(deps, notifier, crate::auto_register::shared_initial())
    }

    /// Used by `ui::run` to share the orchestration loop's
    /// `SharedRegistration` slot with the UI.
    pub fn with_notifier_and_registration(
        deps: AppDeps,
        notifier: Box<dyn Notifier + Send + Sync>,
        registration: crate::auto_register::SharedRegistration,
    ) -> Self {
        let (config_draft, start_minimised_pending) = {
            let cfg = deps.cfg.lock();
            (ConfigDraft::from(&cfg), cfg.start_minimised)
        };
        let vram_total_gb = sys::detect_vram_gb().unwrap_or(0.0);
        Self {
            deps,
            tab: Tab::initial(),
            registration,
            config_draft,
            log_filter: LogFilter::default(),
            about_state: AboutState::default(),
            vram_total_gb,
            last_notified: None,
            notifier,
            notification_prefs: NotificationPrefs::default(),
            tray_variant: TrayVariant::Disconnected,
            quit_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tray: None,
            start_minimised_pending,
        }
    }

    /// Whether the first frame will request a minimised window.
    /// Public for the start-minimised regression tests.
    pub fn start_minimised_pending(&self) -> bool {
        self.start_minimised_pending
    }

    pub fn registration_handle(&self) -> crate::auto_register::SharedRegistration {
        self.registration.clone()
    }

    pub fn attach_tray(&mut self, tray: super::tray_host::TrayHandle) {
        self.tray = Some(tray);
    }

    pub fn quit_requested_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.quit_requested.clone()
    }

    pub fn notification_prefs(&self) -> NotificationPrefs {
        self.notification_prefs
    }

    pub fn set_notification_prefs(&mut self, prefs: NotificationPrefs) {
        self.notification_prefs = prefs;
    }

    pub fn tray_variant(&self) -> TrayVariant {
        self.tray_variant
    }

    fn default_notifier() -> Box<dyn Notifier + Send + Sync> {
        Self::default_notifier_box()
    }

    /// Exposed for `ui::run` which builds a notifier before App::new.
    pub fn default_notifier_box() -> Box<dyn Notifier + Send + Sync> {
        Box::new(super::notifier::DesktopNotifier)
    }

    /// Process any new entries in the recent-jobs ring and emit
    /// notifications according to current prefs.  Idempotent.
    pub fn drain_notifications(&mut self) {
        // `record_recent_job` pushes newest-first, so walk from the
        // front collecting every entry newer than the last one we
        // notified on (identified by `job_id` + `finished_at`).  This
        // stays correct even once the capped ring saturates and its
        // length stops changing.
        let new_entries: Vec<_> = {
            let ring = self.deps.observers.recent_jobs.lock();
            let mut collected = Vec::new();
            for entry in ring.iter() {
                if self
                    .last_notified
                    .as_ref()
                    .is_some_and(|(id, ts)| entry.job_id == *id && entry.finished_at == *ts)
                {
                    break;
                }
                collected.push(entry.clone());
            }
            collected
        };
        if let Some(newest) = new_entries.first() {
            self.last_notified = Some((newest.job_id.clone(), newest.finished_at));
        }
        // `collected` is newest-first; notify oldest-first so the OS
        // notification order matches completion order.
        for entry in new_entries.into_iter().rev() {
            if let NotifyDecision::Show { title, body } = decide(self.notification_prefs, &entry) {
                self.notifier.show(&title, &body);
            }
        }
    }

    /// Recompute the tray variant from live state.  Pushes the new
    /// icon + tooltip to the OS tray when the variant changes.
    pub fn refresh_tray_variant(&mut self) -> TrayVariant {
        let busy = self.deps.busy.load(std::sync::atomic::Ordering::SeqCst);
        let hb = self.deps.observers.last_heartbeat.lock().clone();
        let v = tray::derive_variant(busy, hb.as_ref(), HEARTBEAT_INTERVAL);
        if v != self.tray_variant {
            log_tray_variant_change(self.tray_variant, v);
            // Push the new health colour + tooltip to the OS tray.  The
            // per-platform backend (ksni on Linux, tray-icon on
            // mac/win) surfaces its own failures; here we just forward.
            if let Some(tray) = self.tray.as_mut() {
                tray.set_variant(v);
            }
        }
        self.tray_variant = v;
        v
    }

    /// Shared by both the real `ui` entry point and the headless test
    /// harness so the layout is exercised in tests too.  Takes a
    /// `&mut Ui` rather than a `&Context` to match the new eframe
    /// 0.34 API (`Panel::show_inside`).
    pub fn render(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("tab_bar").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for tab in Tab::ALL {
                    let selected = self.tab == tab;
                    if ui.selectable_label(selected, tab.label()).clicked() {
                        self.tab = tab;
                    }
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Status => self.render_status(ui),
                Tab::Jobs => self.render_jobs(ui),
                Tab::Config => self.render_config(ui),
                Tab::Logs => self.render_logs(ui),
                Tab::About => self.render_about(ui),
            });
        });

        // The background loops mutate shared state asynchronously; ask
        // egui to repaint so updates surface without a user event.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }

    /// Shared housekeeping invoked before every frame's render — keeps
    /// the `ui()` entry point thin.
    fn pre_render(&mut self, ctx: &egui::Context) {
        // Honour `start_minimised` on the first frame.
        if self.start_minimised_pending {
            self.start_minimised_pending = false;
            tracing::info!(
                target: TRACE_TARGET,
                op = "start_minimised",
                "minimising window on startup (config start_minimised)"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        self.drain_notifications();
        self.refresh_tray_variant();

        // Hide-to-tray: intercept the OS close request, keep loops
        // alive, hide the window.  Quit comes from the tray menu.
        if ctx.input(|i| i.viewport().close_requested())
            && !self
                .quit_requested
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            tracing::info!(
                target: TRACE_TARGET,
                op = "hide_to_tray",
                "window close intercepted; hiding to tray (worker keeps running)"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Tray Quit was clicked — stop the loops, then close.
        if self
            .quit_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.deps
                .stop
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Expose the current tab for tests + future tray-state derivation.
    pub fn current_tab(&self) -> Tab {
        self.tab
    }

    /// Switch tab (used by tests + tray menu in Phase 10).
    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn deps(&self) -> &AppDeps {
        &self.deps
    }

    fn render_jobs(&mut self, ui: &mut egui::Ui) {
        let view = jobs_tab::JobsView::build(&self.deps.observers, chrono::Utc::now());
        jobs_tab::render(ui, &view);
    }

    fn render_config(&mut self, ui: &mut egui::Ui) {
        let saved = config_tab::render(
            ui,
            &mut self.config_draft,
            &self.deps.config_path,
            &mut self.notification_prefs,
        );
        // After Save the on-disk file is the new truth; mirror back to
        // the shared `Arc<Mutex<Config>>` so loops see new values on
        // the next tick.
        if saved {
            let mut shared = self.deps.cfg.lock();
            *shared = self.config_draft.current.clone();
        }
    }

    fn render_logs(&mut self, ui: &mut egui::Ui) {
        logs_tab::render(ui, &self.deps.observers.recent_logs, &mut self.log_filter);
    }

    fn render_about(&mut self, ui: &mut egui::Ui) {
        let view = about_tab::AboutView::build(&self.about_state, &self.deps.config_path);
        about_tab::render(
            ui,
            &view,
            &self.about_state,
            &self.deps.tokio,
            &self.deps.config_path,
        );
    }

    fn render_status(&mut self, ui: &mut egui::Ui) {
        let registration_snapshot = self.registration.lock().clone();
        let paused_flag = self.deps.paused.clone();
        let view = {
            let cfg = self.deps.cfg.lock();
            let busy = self.deps.busy.load(std::sync::atomic::Ordering::SeqCst);
            let paused = paused_flag.load(std::sync::atomic::Ordering::SeqCst);
            let hb = self.deps.observers.last_heartbeat.lock().clone();
            let session_state = self.deps.observers.session_state.lock().clone();
            status_tab::StatusView::build(
                &cfg,
                &registration_snapshot,
                busy,
                paused,
                hb.as_ref(),
                self.vram_total_gb,
                &session_state,
            )
        };
        status_tab::render(ui, &view, &paused_flag);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pre_render(&ctx);
        self.render(ui);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::{config::Config, runtime::WorkerObservers};

    fn mock_deps() -> AppDeps {
        // Build a single-thread tokio runtime so `Handle::current()`
        // resolves inside the test process without main.rs being in
        // play.  The runtime is leaked intentionally — the handle
        // stays alive for the duration of the test.
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let handle = RT
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(1)
                    .build()
                    .expect("tokio runtime")
            })
            .handle()
            .clone();
        let cfg = crate::config::shared(Config::default());
        let logs = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let observers = WorkerObservers {
            current_job: Arc::new(Mutex::new(None)),
            recent_jobs: Arc::new(Mutex::new(VecDeque::new())),
            local_jobs: Arc::new(Mutex::new(VecDeque::new())),
            local_api_url: Arc::new(Mutex::new(None)),
            last_heartbeat: Arc::new(Mutex::new(None)),
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        AppDeps {
            cfg,
            logs,
            busy,
            paused,
            observers,
            stop,
            config_path: PathBuf::from("/tmp/studio-worker-test.toml"),
            tokio: handle,
        }
    }

    #[test]
    fn start_minimised_pending_follows_the_config() {
        // Default config: minimised on the first frame.
        let app = App::new(mock_deps());
        assert!(
            app.start_minimised_pending(),
            "default config must request a minimised start"
        );

        // Operator opted out: no minimise request.
        let deps = mock_deps();
        deps.cfg.lock().start_minimised = false;
        let app = App::new(deps);
        assert!(!app.start_minimised_pending());
    }

    #[test]
    fn log_tray_variant_change_emits_structured_transition() {
        use crate::test_support::capture;
        let logs = capture(|| {
            super::log_tray_variant_change(TrayVariant::Disconnected, TrayVariant::Busy);
        });
        assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
        assert!(
            logs.contains("studio_worker::ui::app"),
            "expected app target, got: {logs}"
        );
        assert!(
            logs.contains("op=\"tray_variant\""),
            "expected op field: {logs}"
        );
        assert!(
            logs.contains("from=Disconnected"),
            "expected from field: {logs}"
        );
        assert!(logs.contains("to=Busy"), "expected to field: {logs}");
    }

    #[test]
    fn new_defaults_to_status_tab() {
        let app = App::new(mock_deps());
        assert_eq!(app.current_tab(), Tab::Status);
    }

    #[test]
    fn set_tab_switches() {
        let mut app = App::new(mock_deps());
        app.set_tab(Tab::Logs);
        assert_eq!(app.current_tab(), Tab::Logs);
    }

    /// Headless smoke test: drive one full frame through `render` and
    /// assert it doesn't panic.  Uses `egui::__run_test_ctx` so no
    /// display server is required — runs fine on CI.
    #[test]
    fn render_does_not_panic_under_test_ui() {
        let mut app = App::new(mock_deps());
        egui::__run_test_ui(|ui| {
            app.render(ui);
        });
    }

    #[test]
    fn render_each_tab_does_not_panic() {
        for tab in Tab::ALL {
            let mut app = App::new(mock_deps());
            app.set_tab(tab);
            egui::__run_test_ui(|ui| {
                app.render(ui);
            });
        }
    }

    #[test]
    fn config_tab_does_not_publish_unsaved_edits_to_shared_config() {
        let mut app = App::new(mock_deps());
        app.config_draft.current.api_base_url = "https://unsaved.example".into();

        egui::__run_test_ui(|ui| {
            app.render_config(ui);
        });

        assert_eq!(
            app.deps.cfg.lock().api_base_url,
            Config::default().api_base_url,
            "editing the draft must not affect the live runtime config until Save succeeds"
        );
    }

    fn completed_recent_job(id: &str) -> crate::runtime::RecentJob {
        let now = chrono::Utc::now();
        crate::runtime::RecentJob {
            job_id: id.into(),
            kind: crate::types::TaskKind::Image,
            model: "synthetic".into(),
            prompt: "p".into(),
            outcome: crate::runtime::JobOutcome::Completed,
            started_at: now,
            finished_at: now,
        }
    }

    /// Shared handle into a `CapturingNotifier`'s recorded
    /// (title, body) pairs.
    type Captured = Arc<Mutex<Vec<(String, String)>>>;

    fn app_with_capturing_notifier(deps: AppDeps) -> (App, Captured) {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let notifier = Box::new(crate::ui::notifier::CapturingNotifier {
            captured: captured.clone(),
        });
        let mut app = App::with_notifier(deps, notifier);
        app.set_notification_prefs(NotificationPrefs {
            on_completion: true,
            on_failure: true,
        });
        (app, captured)
    }

    #[test]
    fn drain_notifications_fires_for_each_new_completed_job() {
        let deps = mock_deps();
        let observers = deps.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);

        crate::runtime::record_recent_job(&observers, completed_recent_job("a"));
        crate::runtime::record_recent_job(&observers, completed_recent_job("b"));
        app.drain_notifications();

        assert_eq!(captured.lock().len(), 2);
    }

    #[test]
    fn drain_notifications_is_idempotent_without_new_jobs() {
        let deps = mock_deps();
        let observers = deps.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);

        crate::runtime::record_recent_job(&observers, completed_recent_job("a"));
        app.drain_notifications();
        app.drain_notifications();
        app.drain_notifications();

        assert_eq!(
            captured.lock().len(),
            1,
            "re-draining with no new jobs must not re-notify"
        );
    }

    #[test]
    fn drain_notifications_fires_after_recent_jobs_ring_saturates() {
        let deps = mock_deps();
        let observers = deps.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);

        // Fill the capped ring past `RECENT_JOBS_CAP` so its length
        // pins at the cap and a length-based "added" comparison can no
        // longer detect new arrivals.
        for i in 0..(crate::runtime::RECENT_JOBS_CAP + 5) {
            crate::runtime::record_recent_job(
                &observers,
                completed_recent_job(&format!("warm-{i}")),
            );
        }
        app.drain_notifications();
        captured.lock().clear();

        // A brand-new job after saturation must still raise a
        // notification — the regression this test guards.
        crate::runtime::record_recent_job(&observers, completed_recent_job("after-saturation"));
        app.drain_notifications();

        let shown = captured.lock();
        assert_eq!(
            shown.len(),
            1,
            "a job completing after the ring saturates must still notify"
        );
        assert!(
            shown[0].1.contains("image"),
            "notification body should describe the new job, got: {:?}",
            shown[0]
        );
    }
}
