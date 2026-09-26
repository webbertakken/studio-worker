//! The eframe `App`: a client of the daemon.  It renders the [`Replica`]
//! the poller keeps fresh and sends the operator's actions through the
//! [`ActionRunner`]; it never runs a job itself.

use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::{
    daemon_api::EditableConfig,
    daemon_link::{Action, LinkState, Replica},
    runtime::HEARTBEAT_INTERVAL,
};

use super::{
    actions::ActionRunner,
    notifier::{decide, NotificationPrefs, Notifier, NotifyDecision},
    tab::Tab,
    tabs::{
        about::{self as about_tab, AboutState, UpdateFeed},
        config::{self as config_tab, ConfigDraft},
        jobs::{self as jobs_tab, JobsContext, ThumbnailTextures},
        logs::{self as logs_tab, LogFilter},
        models::{self as models_tab, ModelAction, ModelRow},
        status::{self as status_tab, StatusAction},
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

/// The tray colour for the daemon's state: disconnected whenever the link
/// is down, else derived from busy + heartbeat.
pub fn tray_variant_for(link: &LinkState, replica: &Replica) -> TrayVariant {
    if !link.is_connected() {
        return TrayVariant::Disconnected;
    }
    let busy =
        replica.busy.load(Ordering::SeqCst) || !replica.observers.active_jobs.lock().is_empty();
    let hb = replica.observers.last_heartbeat.lock().clone();
    tray::derive_variant(busy, hb.as_ref(), HEARTBEAT_INTERVAL)
}

/// Everything `App` needs to render and act on the world.
pub struct AppDeps {
    pub replica: Replica,
    /// Minimise the window on its first frame (the config's
    /// `start_minimised`, read before the daemon answers).
    pub start_minimised: bool,
    pub actions: ActionRunner,
    pub config_path: PathBuf,
    pub tokio: Handle,
}

/// A config save on its way to the daemon.
type PendingSave = Arc<Mutex<Option<Result<EditableConfig, String>>>>;

pub struct App {
    deps: AppDeps,
    tab: Tab,
    config_draft: ConfigDraft,
    pending_save: PendingSave,
    log_filter: LogFilter,
    about_state: AboutState,
    textures: ThumbnailTextures,
    /// `STUDIO_WORKER_UI_JOB`: a job to select once it shows up.
    initial_job: Option<String>,
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
    /// Quit is under way: the daemon was told to stop, the window closes.
    quitting: bool,
    tray: Option<super::tray_host::TrayHandle>,
    /// One-shot request to minimise the window on the first frame
    /// (config `start_minimised`, default true).  Minimised to the
    /// taskbar — not hidden — so the window stays reachable even when
    /// no tray host is available.
    start_minimised_pending: bool,
}

impl App {
    pub fn new(deps: AppDeps) -> Self {
        Self::with_notifier(deps, Self::default_notifier_box())
    }

    /// Used by tests to inject a `CapturingNotifier`.
    pub fn with_notifier(deps: AppDeps, notifier: Box<dyn Notifier + Send + Sync>) -> Self {
        let config_draft = ConfigDraft::from(&deps.replica.cfg.lock());
        let start_minimised_pending = deps.start_minimised;
        Self {
            deps,
            tab: Tab::initial(),
            config_draft,
            pending_save: Arc::default(),
            log_filter: LogFilter::default(),
            about_state: AboutState::default(),
            textures: ThumbnailTextures::default(),
            initial_job: std::env::var("STUDIO_WORKER_UI_JOB").ok(),
            last_notified: None,
            notifier,
            notification_prefs: NotificationPrefs::default(),
            tray_variant: TrayVariant::Disconnected,
            quit_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quitting: false,
            tray: None,
            start_minimised_pending,
        }
    }

    /// Whether the first frame will request a minimised window.
    pub fn start_minimised_pending(&self) -> bool {
        self.start_minimised_pending
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

    /// Exposed for `ui::run` which builds a notifier before App::new.
    pub fn default_notifier_box() -> Box<dyn Notifier + Send + Sync> {
        Box::new(super::notifier::DesktopNotifier)
    }

    /// Process any new entries in the recent-jobs ring and emit
    /// notifications according to current prefs.  Idempotent.
    pub fn drain_notifications(&mut self) {
        // `recent_jobs` is newest-first, so walk from the front collecting
        // every entry newer than the last one we notified on (identified
        // by `job_id` + `finished_at`).
        let new_entries: Vec<_> = {
            let ring = self.deps.replica.observers.recent_jobs.lock();
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
        // Notify oldest-first so the OS order matches completion order.
        for entry in new_entries.into_iter().rev() {
            if let NotifyDecision::Show { title, body } = decide(self.notification_prefs, &entry) {
                self.notifier.show(&title, &body);
            }
        }
    }

    /// Recompute the tray variant from live state.  Pushes the new
    /// icon + tooltip to the OS tray when the variant changes.
    pub fn refresh_tray_variant(&mut self) -> TrayVariant {
        let link = self.deps.replica.link.lock().clone();
        let v = tray_variant_for(&link, &self.deps.replica);
        if v != self.tray_variant {
            log_tray_variant_change(self.tray_variant, v);
            if let Some(tray) = self.tray.as_mut() {
                tray.set_variant(v);
            }
        }
        self.tray_variant = v;
        v
    }

    /// Shared by the real `ui` entry point and the headless tests.
    pub fn render(&mut self, ui: &mut egui::Ui) {
        let link = self.deps.replica.link.lock().clone();
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
            ui.add_space(2.0);
            self.render_status_line(ui, &link);
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if !link.is_connected() && self.tab != Tab::About {
                    render_unreachable(ui, &link, &self.deps.config_path);
                    return;
                }
                match self.tab {
                    Tab::Status => self.render_status(ui),
                    Tab::Jobs => self.render_jobs(ui),
                    Tab::Models => self.render_models(ui),
                    Tab::Config => self.render_config(ui),
                    Tab::Logs => self.render_logs(ui),
                    Tab::About => self.render_about(ui),
                }
            });
        });

        // The poller updates the replica asynchronously; keep repainting so
        // durations tick and new state shows without a user event.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    /// One line, always present (so nothing shifts): the link, and the
    /// result of the last action.
    fn render_status_line(&self, ui: &mut egui::Ui, link: &LinkState) {
        ui.horizontal(|ui| {
            let colour = match link {
                LinkState::Connected { .. } => egui::Color32::LIGHT_GREEN,
                LinkState::Connecting | LinkState::Starting { .. } => {
                    egui::Color32::from_rgb(232, 168, 56)
                }
                LinkState::Unreachable { .. } => egui::Color32::LIGHT_RED,
            };
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, colour);
            ui.label(egui::RichText::new(link.summary()).small());
            if let Some(feedback) = self.deps.actions.feedback.lock().clone() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let colour = if feedback.ok {
                        egui::Color32::from_gray(190)
                    } else {
                        egui::Color32::LIGHT_RED
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{} \u{00b7} {}",
                            feedback.at.with_timezone(&chrono::Local).format("%H:%M:%S"),
                            feedback.text
                        ))
                        .small()
                        .color(colour),
                    );
                });
            }
        });
    }

    /// Shared housekeeping invoked before every frame's render.
    fn pre_render(&mut self, ctx: &egui::Context) {
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
        self.take_save_result();

        // Hide-to-tray: intercept the OS close request and hide the window.
        // The daemon keeps running either way; Quit comes from the tray.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            tracing::info!(
                target: TRACE_TARGET,
                op = "hide_to_tray",
                "window close intercepted; hiding to tray (the daemon keeps running)"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Tray Quit: stop the daemon, then close the UI.
        if self.quit_requested.load(Ordering::SeqCst) && !self.quitting {
            self.quitting = true;
            tracing::info!(
                target: TRACE_TARGET,
                op = "quit",
                "quit requested; stopping the daemon and closing the tray UI"
            );
            self.deps.actions.run_and_wait(Action::Shutdown);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Expose the current tab for tests + future tray-state derivation.
    pub fn current_tab(&self) -> Tab {
        self.tab
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn deps(&self) -> &AppDeps {
        &self.deps
    }

    fn render_jobs(&mut self, ui: &mut egui::Ui) {
        let replica = &self.deps.replica;
        let view = jobs_tab::JobsView::build(&replica.observers, chrono::Utc::now());
        if let Some(spec) = &self.initial_job {
            if let Some(id) = jobs_tab::resolve_initial_selection(spec, &view) {
                *replica.selected_job.lock() = Some(id);
                self.initial_job = None;
            }
        }
        let selected = replica.selected_job.lock().clone();
        let log = replica.selected_log.lock().clone();
        let changed = jobs_tab::render(
            ui,
            &view,
            JobsContext {
                thumbnails: &replica.observers.thumbnails,
                textures: &mut self.textures,
                selected: selected.as_deref(),
                log: log.as_ref(),
            },
        );
        if let Some(selection) = changed {
            *replica.selected_log.lock() = None;
            *replica.selected_job.lock() = selection;
        }
    }

    fn render_models(&mut self, ui: &mut egui::Ui) {
        let rows: Vec<ModelRow> = self
            .deps
            .replica
            .models
            .lock()
            .iter()
            .map(ModelRow::from_entry)
            .collect();
        match models_tab::render(ui, &rows) {
            Some(ModelAction::Load(id)) => self.deps.actions.run(Action::Load(id)),
            Some(ModelAction::Unload(id)) => self.deps.actions.run(Action::Unload(id)),
            None => {}
        }
    }

    fn render_config(&mut self, ui: &mut egui::Ui) {
        let live = self.deps.replica.cfg.lock().clone();
        self.config_draft.follow(&live);
        if let Some(edit) = config_tab::render(
            ui,
            &mut self.config_draft,
            &self.deps.config_path,
            &mut self.notification_prefs,
        ) {
            let slot = self.pending_save.clone();
            let path = self.deps.config_path.clone();
            let edit = EditableConfig::from_config(&edit);
            std::thread::spawn(move || {
                *slot.lock() = Some(crate::daemon_link::save_config(&path, &edit));
            });
        }
    }

    /// Apply the daemon's answer to a Save, once it arrived.
    fn take_save_result(&mut self) {
        let Some(result) = self.pending_save.lock().take() else {
            return;
        };
        match result {
            Ok(saved) => {
                let mut cfg = self.deps.replica.cfg.lock();
                saved.apply_to(&mut cfg);
                self.config_draft.saved(&cfg);
            }
            Err(err) => self.config_draft.save_failed(err),
        }
    }

    fn render_logs(&mut self, ui: &mut egui::Ui) {
        logs_tab::render(
            ui,
            &self.deps.replica.observers.recent_logs,
            &mut self.log_filter,
        );
    }

    fn render_about(&mut self, ui: &mut egui::Ui) {
        let daemon_version = self
            .deps
            .replica
            .status
            .lock()
            .as_ref()
            .map(|s| s.version.clone());
        let view =
            about_tab::AboutView::build(&self.about_state, &self.deps.config_path, daemon_version);
        let feed = {
            let cfg = self.deps.replica.cfg.lock();
            UpdateFeed {
                url: cfg.auto_update_feed.clone(),
                prerelease: cfg.auto_update_prerelease,
            }
        };
        about_tab::render(ui, &view, &self.about_state, &self.deps.tokio, &feed);
    }

    fn render_status(&mut self, ui: &mut egui::Ui) {
        let replica = &self.deps.replica;
        let view = {
            let cfg = replica.cfg.lock();
            let registration = replica.registration.lock().clone();
            let hb = replica.observers.last_heartbeat.lock().clone();
            let session_state = replica.observers.session_state.lock().clone();
            let gpu = replica.observers.gpu_runtime.lock().clone();
            let vram_total_gb = replica
                .status
                .lock()
                .as_ref()
                .map_or(0.0, |s| s.vram_total_gb);
            status_tab::StatusView::build(
                &cfg,
                replica.registered(),
                &registration,
                replica.busy.load(Ordering::SeqCst),
                replica.paused.load(Ordering::SeqCst),
                hb.as_ref(),
                vram_total_gb,
                &session_state,
                gpu.as_ref(),
            )
        };
        match status_tab::render(ui, &view) {
            Some(StatusAction::SetPaused(paused)) => {
                self.deps.actions.run(Action::SetPaused(paused))
            }
            Some(StatusAction::ResetRegistration) => {
                self.deps.actions.run(Action::ResetRegistration)
            }
            None => {}
        }
    }
}

/// What the window shows instead of the tabs while the daemon does not
/// answer: never stale data.
fn render_unreachable(ui: &mut egui::Ui, link: &LinkState, config_path: &std::path::Path) {
    ui.heading("Worker daemon not reachable");
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(link.summary());
    });
    ui.add_space(8.0);
    let detail = match link {
        LinkState::Starting { error } | LinkState::Unreachable { error, .. } => error.as_str(),
        _ => "",
    };
    if !detail.is_empty() {
        ui.label(
            egui::RichText::new(detail)
                .monospace()
                .color(egui::Color32::from_gray(180)),
        );
        ui.add_space(8.0);
    }
    ui.label(format!(
        "The tray UI shows what the daemon (`studio-worker run`) does.  It starts one when \
         none is running and keeps retrying.  A daemon it starts writes its output to {}.",
        crate::daemon_link::daemon_log_path(config_path).display()
    ));
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
    use super::*;
    use crate::{config::Config, daemon_link::Replica};

    fn tokio_handle() -> Handle {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("tokio runtime")
        })
        .handle()
        .clone()
    }

    fn mock_deps() -> AppDeps {
        let replica = Replica::default();
        let config_path = PathBuf::from("/tmp/studio-worker-test/config.toml");
        AppDeps {
            actions: ActionRunner::new(config_path.clone(), replica.clone()),
            replica,
            start_minimised: true,
            config_path,
            tokio: tokio_handle(),
        }
    }

    fn connected(deps: &AppDeps) {
        *deps.replica.link.lock() = LinkState::Connected {
            url: "http://127.0.0.1:4787".into(),
            version: crate::AGENT_VERSION.into(),
        };
    }

    #[test]
    fn start_minimised_pending_follows_the_config() {
        let app = App::new(mock_deps());
        assert!(app.start_minimised_pending());

        let deps = AppDeps {
            start_minimised: false,
            ..mock_deps()
        };
        let app = App::new(deps);
        assert!(!app.start_minimised_pending());
    }

    #[test]
    fn log_tray_variant_change_emits_structured_transition() {
        use crate::test_support::capture;
        let logs = capture(|| {
            super::log_tray_variant_change(TrayVariant::Disconnected, TrayVariant::Busy);
        });
        assert!(logs.contains("studio_worker::ui::app"), "{logs}");
        assert!(logs.contains("op=\"tray_variant\""), "{logs}");
        assert!(logs.contains("from=Disconnected"), "{logs}");
        assert!(logs.contains("to=Busy"), "{logs}");
    }

    #[test]
    fn the_tray_is_disconnected_whenever_the_link_is_down() {
        let replica = Replica::default();
        replica.busy.store(true, Ordering::SeqCst);
        assert_eq!(
            tray_variant_for(&LinkState::Connecting, &replica),
            TrayVariant::Disconnected
        );
        let link = LinkState::Connected {
            url: "u".into(),
            version: "v".into(),
        };
        assert_eq!(tray_variant_for(&link, &replica), TrayVariant::Busy);
    }

    #[test]
    fn a_running_lane_job_makes_the_tray_busy() {
        let replica = Replica::default();
        replica
            .observers
            .active_jobs
            .lock()
            .push(crate::runtime::CurrentJob {
                job_id: "l".into(),
                kind: crate::types::TaskKind::Llm,
                model: "m".into(),
                prompt: String::new(),
                started_at: chrono::Utc::now(),
                source: crate::runtime::JobSource::Lane,
            });
        let link = LinkState::Connected {
            url: "u".into(),
            version: "v".into(),
        };
        assert_eq!(tray_variant_for(&link, &replica), TrayVariant::Busy);
    }

    #[test]
    fn new_defaults_to_status_tab() {
        let app = App::new(mock_deps());
        assert_eq!(app.current_tab(), Tab::Status);
    }

    #[test]
    fn render_each_tab_connected_and_not_does_not_panic() {
        for tab in Tab::ALL {
            for link_up in [false, true] {
                let deps = mock_deps();
                if link_up {
                    connected(&deps);
                }
                let mut app = App::new(deps);
                app.set_tab(tab);
                egui::__run_test_ui(|ui| app.render(ui));
            }
        }
    }

    #[test]
    fn a_save_answer_becomes_the_draft_baseline() {
        let deps = mock_deps();
        let mut app = App::new(deps);
        app.config_draft.current.vram_threshold_gb = 2.0;
        app.config_draft.pending = true;
        let mut saved = EditableConfig::from_config(&Config::default());
        saved.vram_threshold_gb = 2.0;
        *app.pending_save.lock() = Some(Ok(saved));
        app.take_save_result();
        assert!(!app.config_draft.dirty());
        assert_eq!(app.deps.replica.cfg.lock().vram_threshold_gb, 2.0);

        app.config_draft.current.vram_threshold_gb = 3.0;
        *app.pending_save.lock() = Some(Err("invalid config".into()));
        app.take_save_result();
        assert_eq!(
            app.config_draft.last_save_error.as_deref(),
            Some("invalid config")
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
            source: crate::runtime::JobSource::Studio,
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
        let observers = deps.replica.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);
        crate::runtime::record_recent_job(&observers, completed_recent_job("a"));
        crate::runtime::record_recent_job(&observers, completed_recent_job("b"));
        app.drain_notifications();
        assert_eq!(captured.lock().len(), 2);
    }

    #[test]
    fn drain_notifications_is_idempotent_without_new_jobs() {
        let deps = mock_deps();
        let observers = deps.replica.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);
        crate::runtime::record_recent_job(&observers, completed_recent_job("a"));
        app.drain_notifications();
        app.drain_notifications();
        assert_eq!(captured.lock().len(), 1);
    }

    #[test]
    fn drain_notifications_fires_after_recent_jobs_ring_saturates() {
        let deps = mock_deps();
        let observers = deps.replica.observers.clone();
        let (mut app, captured) = app_with_capturing_notifier(deps);
        for i in 0..(crate::runtime::RECENT_JOBS_CAP + 5) {
            crate::runtime::record_recent_job(
                &observers,
                completed_recent_job(&format!("warm-{i}")),
            );
        }
        app.drain_notifications();
        captured.lock().clear();
        crate::runtime::record_recent_job(&observers, completed_recent_job("after-saturation"));
        app.drain_notifications();
        let shown = captured.lock();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].1.contains("image"), "{:?}", shown[0]);
    }
}
