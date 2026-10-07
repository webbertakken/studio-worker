//! The eframe `App`: a client of the daemon.  It renders the [`Replica`]
//! the poller keeps fresh inside the window's chrome (rail, pulse header,
//! status bar) and sends the operator's actions through the
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
};

use super::{
    actions::ActionRunner,
    chrome,
    notifier::{decide, NotificationPrefs, Notifier, NotifyDecision},
    page::Page,
    pages::{
        config::{self as config_page, ConfigDraft},
        jobs::{self as jobs_page, JobsContext, JobsState},
        logs::{self as logs_page, LogFilter},
        models::{self as models_page, ModelAction, ModelsView},
        worker::{self as worker_page, AboutState, UpdateFeed, WorkerAction},
    },
    prefs::{self, UiPrefs},
    pulse::{Pulse, PulseInputs},
    theme::{self, Palette, ThemeChoice},
    tray::{self, TrayVariant},
};

/// Tracing target for App-level lifecycle + tray events.  Stable so
/// operators can filter with `RUST_LOG=studio_worker::ui::app=info`.
const TRACE_TARGET: &str = "studio_worker::ui::app";

/// How often the window repaints while running work glows (≈ 20 fps).
pub const GLOW_FRAME: Duration = Duration::from_millis(50);
/// How often it repaints otherwise, so durations tick.
pub const IDLE_FRAME: Duration = Duration::from_millis(500);
/// Widest the text-heavy pages grow, in points: lines stay readable.
pub const READING_WIDTH: f32 = 1080.0;

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

/// The tray colour: what the header's pulse says ([`tray::variant_of`]).
pub fn tray_variant_for(link: &LinkState, replica: &Replica) -> TrayVariant {
    tray::variant_of(&pulse_of(link, replica, chrono::Utc::now()))
}

/// The pulse the header shows, from the replica.
pub fn pulse_of(link: &LinkState, replica: &Replica, now: chrono::DateTime<chrono::Utc>) -> Pulse {
    let registration = replica.registration.lock().clone();
    let session = replica.observers.session_state.lock().clone();
    let active = replica.observers.active_jobs.lock().clone();
    let models = replica.models.lock().clone();
    let vram_total_gb = replica
        .status
        .lock()
        .as_ref()
        .map_or(0.0, |s| s.vram_total_gb);
    Pulse::build(PulseInputs {
        link,
        registered: replica.registered(),
        registration: &registration,
        session: &session,
        busy: replica.busy.load(Ordering::SeqCst),
        paused: replica.paused.load(Ordering::SeqCst),
        active: &active,
        models: &models,
        vram_total_gb,
        now,
    })
}

/// Everything `App` needs to render and act on the world.
pub struct AppDeps {
    pub replica: Replica,
    /// Minimise the window on its first frame (the config's
    /// `start_minimised`, read before the daemon answers).
    pub start_minimised: bool,
    /// Make the tray UI's login entry match `auto_start`; called with the
    /// saved value after every successful Config save.
    pub login_entry: LoginEntry,
    pub actions: ActionRunner,
    pub config_path: PathBuf,
    pub tokio: Handle,
}

/// Installs (`true`) or removes (`false`) the tray UI's login entry.
pub type LoginEntry = Arc<dyn Fn(bool) + Send + Sync>;

/// A config save on its way to the daemon.
type PendingSave = Arc<Mutex<Option<Result<EditableConfig, String>>>>;

pub struct App {
    deps: AppDeps,
    page: Page,
    config_draft: ConfigDraft,
    pending_save: PendingSave,
    log_filter: LogFilter,
    about_state: AboutState,
    jobs: JobsState,
    /// `STUDIO_WORKER_UI_JOB`: a job to select once it shows up.
    initial_job: Option<String>,
    /// Identity (`job_id` + `finished_at`) of the newest recent-job we
    /// have already raised a notification for.  Tracking identity
    /// rather than ring length means a saturated, capped
    /// `recent_jobs` ring (whose length pins at `RECENT_JOBS_CAP`)
    /// can't make new arrivals invisible.
    last_notified: Option<(String, chrono::DateTime<chrono::Utc>)>,
    notifier: Box<dyn Notifier + Send + Sync>,
    /// The window's own preferences, stored in `ui.toml`.
    prefs: UiPrefs,
    prefs_path: PathBuf,
    /// The theme last handed to egui; `None` until the first frame.
    applied_theme: Option<ThemeChoice>,
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
        let prefs_path = prefs::path_for(&deps.config_path);
        let prefs = prefs::load(&prefs_path);
        Self {
            deps,
            page: Page::initial(),
            config_draft,
            pending_save: Arc::default(),
            log_filter: LogFilter::default(),
            about_state: AboutState::default(),
            jobs: JobsState::default(),
            initial_job: std::env::var("STUDIO_WORKER_UI_JOB").ok(),
            last_notified: None,
            notifier,
            prefs,
            prefs_path,
            applied_theme: None,
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
        self.prefs.notifications()
    }

    pub fn set_notification_prefs(&mut self, prefs: NotificationPrefs) {
        self.prefs.notify_on_completion = prefs.on_completion;
        self.prefs.notify_on_failure = prefs.on_failure;
    }

    pub fn prefs(&self) -> UiPrefs {
        self.prefs
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
        let prefs = self.notification_prefs();
        for entry in new_entries.into_iter().rev() {
            if let NotifyDecision::Show { title, body } = decide(prefs, &entry) {
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

    /// Hand the theme to egui when it changed.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme != Some(self.prefs.theme) {
            theme::apply(ctx, self.prefs.theme);
            self.applied_theme = Some(self.prefs.theme);
        }
    }

    /// Shared by the real `ui` entry point and the headless tests.
    pub fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        if let Some(page) = chrome::page_shortcut(&ctx) {
            self.page = page;
        }
        let link = self.deps.replica.link.lock().clone();
        let pulse = pulse_of(&link, &self.deps.replica, chrono::Utc::now());
        let glow = theme::breath(ui.input(|i| i.time), self.prefs.reduce_motion);
        let p = *Palette::of_ui(ui);
        let chrome_frame = egui::Frame::new().fill(p.chrome);

        egui::Panel::left("rail")
            .exact_size(chrome::RAIL_WIDTH)
            .resizable(false)
            .frame(chrome_frame)
            .show_inside(ui, |ui| {
                if let Some(page) = chrome::rail(ui, self.page, crate::AGENT_VERSION) {
                    self.page = page;
                }
            });
        egui::Panel::top("pulse")
            .exact_size(chrome::HEADER_HEIGHT)
            .resizable(false)
            .frame(chrome_frame.inner_margin(egui::Margin::symmetric(20, 8)))
            .show_inside(ui, |ui| {
                if let Some(paused) = chrome::header(ui, &pulse, glow) {
                    self.deps.actions.run(Action::SetPaused(paused));
                }
            });
        egui::Panel::bottom("status")
            .exact_size(chrome::STATUS_BAR_HEIGHT)
            .resizable(false)
            .frame(chrome_frame.inner_margin(egui::Margin::symmetric(16, 0)))
            .show_inside(ui, |ui| {
                let feedback = self.deps.actions.feedback.lock().clone();
                chrome::status_bar(ui, &link.summary(), feedback.as_ref());
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.page)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show_inside(ui, |ui| self.render_page(ui, &link, &pulse, glow));

        // The poller updates the replica asynchronously; keep repainting so
        // durations tick, and more often while running work glows.
        let frame = if pulse.activity.glows() && !self.prefs.reduce_motion {
            GLOW_FRAME
        } else {
            IDLE_FRAME
        };
        ctx.request_repaint_after(frame);
    }

    fn render_page(&mut self, ui: &mut egui::Ui, link: &LinkState, pulse: &Pulse, glow: f32) {
        if !link.is_connected() {
            let detail = match link {
                LinkState::Starting { error } | LinkState::Unreachable { error, .. } => {
                    error.as_str()
                }
                _ => "",
            };
            let log = crate::daemon_link::daemon_log_path(&self.deps.config_path);
            let offline = worker_page::Offline {
                summary: link.summary(),
                detail: detail.to_string(),
                daemon_log: log,
            };
            if self.page == Page::Worker {
                reading_column(ui, "worker", |ui| {
                    self.render_worker(ui, pulse, glow, Some(&offline))
                });
            } else {
                reading_column(ui, "unreachable", |ui| {
                    worker_page::unreachable_card(ui, &offline)
                });
            }
            return;
        }
        match self.page {
            Page::Jobs => self.render_jobs(ui),
            Page::Models => reading_column(ui, "models", |ui| self.render_models(ui)),
            Page::Worker => {
                reading_column(ui, "worker", |ui| self.render_worker(ui, pulse, glow, None))
            }
            Page::Logs => logs_page::render(
                ui,
                &self.deps.replica.observers.recent_logs,
                &mut self.log_filter,
            ),
            Page::Config => self.render_config(ui),
        }
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

    /// The page the window shows.
    pub fn current_page(&self) -> Page {
        self.page
    }

    pub fn set_page(&mut self, page: Page) {
        self.page = page;
    }

    pub fn deps(&self) -> &AppDeps {
        &self.deps
    }

    fn render_jobs(&mut self, ui: &mut egui::Ui) {
        let replica = &self.deps.replica;
        let view = jobs_page::JobsView::build(&replica.observers);
        if let Some(spec) = &self.initial_job {
            if let Some(id) = jobs_page::resolve_initial_selection(spec, &view) {
                *replica.selected_job.lock() = Some(id);
                self.initial_job = None;
            }
        }
        let selected = replica.selected_job.lock().clone();
        let log = replica.selected_log.lock().clone();
        let changed = jobs_page::render(
            ui,
            &view,
            JobsContext {
                thumbnails: &replica.observers.thumbnails,
                state: &mut self.jobs,
                selected: selected.as_deref(),
                log: log.as_ref(),
                paused: replica.paused.load(Ordering::SeqCst),
                reduce_motion: self.prefs.reduce_motion,
            },
        );
        if let Some(selection) = changed {
            *replica.selected_log.lock() = None;
            *replica.selected_job.lock() = selection;
        }
    }

    fn render_models(&mut self, ui: &mut egui::Ui) {
        let vram_total_gb = self
            .deps
            .replica
            .status
            .lock()
            .as_ref()
            .map_or(0.0, |s| s.vram_total_gb);
        let view = ModelsView::build(&self.deps.replica.models.lock(), vram_total_gb);
        match models_page::render(ui, &view) {
            Some(ModelAction::Load(id)) => self.deps.actions.run(Action::Load(id)),
            Some(ModelAction::Unload(id)) => self.deps.actions.run(Action::Unload(id)),
            None => {}
        }
    }

    fn render_worker(
        &mut self,
        ui: &mut egui::Ui,
        pulse: &Pulse,
        glow: f32,
        offline: Option<&worker_page::Offline>,
    ) {
        let replica = &self.deps.replica;
        let status = replica.status.lock().clone();
        let facts = offline.is_none().then(|| {
            let cfg = replica.cfg.lock();
            let registration = replica.registration.lock().clone();
            let session = replica.observers.session_state.lock().clone();
            let hb = replica.observers.last_heartbeat.lock().clone();
            let gpu = replica.observers.gpu_runtime.lock().clone();
            worker_page::WorkerFacts::build(worker_page::FactsInputs {
                cfg: &cfg,
                registered: replica.registered(),
                registration: &registration,
                session: &session,
                heartbeat: hb.as_ref(),
                gpu: gpu.as_ref(),
                vram_total_gb: status.as_ref().map_or(0.0, |s| s.vram_total_gb),
                held_gb: pulse.gpu.map_or(0.0, |g| g.held_gb),
                local_api_url: replica.observers.local_api_url.lock().clone(),
            })
        });
        let about = worker_page::AboutView::build(
            &self.about_state,
            &self.deps.config_path,
            status.map(|s| s.version),
        );
        let feed = {
            let cfg = replica.cfg.lock();
            UpdateFeed {
                url: cfg.auto_update_feed.clone(),
                prerelease: cfg.auto_update_prerelease,
            }
        };
        let action = worker_page::render(
            ui,
            worker_page::WorkerContext {
                activity: &pulse.activity,
                paused: pulse.paused,
                facts: facts.as_ref(),
                offline,
                about: &about,
                about_state: &self.about_state,
                tokio: &self.deps.tokio,
                feed: &feed,
                glow,
            },
        );
        match action {
            Some(WorkerAction::SetPaused(paused)) => {
                self.deps.actions.run(Action::SetPaused(paused))
            }
            Some(WorkerAction::ResetRegistration) => {
                self.deps.actions.run(Action::ResetRegistration)
            }
            None => {}
        }
    }

    fn render_config(&mut self, ui: &mut egui::Ui) {
        let live = self.deps.replica.cfg.lock().clone();
        self.config_draft.follow(&live);
        let outcome = config_page::render(
            ui,
            &mut self.config_draft,
            &self.deps.config_path,
            &mut self.prefs,
        );
        if outcome.prefs_changed {
            // A failed save is logged; the preference still applies now.
            let _ = prefs::save(&self.prefs_path, &self.prefs);
        }
        if let Some(edit) = outcome.save {
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
                (self.deps.login_entry)(saved.auto_start);
                let mut cfg = self.deps.replica.cfg.lock();
                saved.apply_to(&mut cfg);
                self.config_draft.saved(&cfg);
            }
            Err(err) => self.config_draft.save_failed(err),
        }
    }
}

/// A vertically scrolling column no wider than [`READING_WIDTH`].
fn reading_column(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_max_width(READING_WIDTH);
            add(ui);
        });
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
            login_entry: Arc::new(|_| {}),
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
    fn the_window_opens_on_jobs() {
        let app = App::new(mock_deps());
        assert_eq!(app.current_page(), Page::Jobs);
    }

    #[test]
    fn every_page_renders_connected_and_not_in_both_themes() {
        for page in Page::ALL {
            for link_up in [false, true] {
                for theme in [ThemeChoice::Dark, ThemeChoice::Light] {
                    let deps = mock_deps();
                    if link_up {
                        connected(&deps);
                        seed(&deps.replica);
                    }
                    let mut app = App::new(deps);
                    app.prefs.theme = theme;
                    app.set_page(page);
                    egui::__run_test_ui(|ui| app.render(ui));
                    assert_eq!(app.applied_theme, Some(theme));
                }
            }
        }
    }

    /// A replica with a running job, a finished one and a loaded model.
    fn seed(replica: &Replica) {
        let now = chrono::Utc::now();
        replica
            .observers
            .active_jobs
            .lock()
            .push(crate::runtime::CurrentJob {
                job_id: "run".into(),
                kind: crate::types::TaskKind::Image,
                model: "sd".into(),
                prompt: "a fox".into(),
                started_at: now,
                source: crate::runtime::JobSource::Studio,
            });
        crate::runtime::record_recent_job(&replica.observers, completed_recent_job("done"));
        *replica.selected_job.lock() = Some("done".into());
    }

    #[test]
    fn the_pulse_reads_the_replica() {
        let deps = mock_deps();
        connected(&deps);
        seed(&deps.replica);
        let link = deps.replica.link.lock().clone();
        let pulse = pulse_of(&link, &deps.replica, chrono::Utc::now());
        assert!(pulse.activity.glows());
        assert!(pulse.can_pause);
    }

    #[test]
    fn window_preferences_load_from_and_save_next_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        prefs::save(
            &prefs::path_for(&config_path),
            &UiPrefs {
                theme: ThemeChoice::Light,
                reduce_motion: true,
                ..UiPrefs::default()
            },
        )
        .unwrap();
        let deps = AppDeps {
            config_path: config_path.clone(),
            ..mock_deps()
        };
        let mut app = App::new(deps);
        assert_eq!(app.prefs().theme, ThemeChoice::Light);
        assert!(app.prefs().reduce_motion);
        app.set_notification_prefs(NotificationPrefs {
            on_completion: true,
            on_failure: false,
        });
        assert!(app.notification_prefs().on_completion);
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

    #[test]
    fn a_saved_config_syncs_the_login_entry_and_a_refused_one_does_not() {
        let calls: Arc<Mutex<Vec<bool>>> = Arc::default();
        let seen = calls.clone();
        let deps = AppDeps {
            login_entry: Arc::new(move |enabled| seen.lock().push(enabled)),
            ..mock_deps()
        };
        let mut app = App::new(deps);
        let mut saved = EditableConfig::from_config(&Config::default());
        saved.auto_start = false;
        *app.pending_save.lock() = Some(Ok(saved.clone()));
        app.take_save_result();
        saved.auto_start = true;
        *app.pending_save.lock() = Some(Ok(saved));
        app.take_save_result();
        *app.pending_save.lock() = Some(Err("invalid config".into()));
        app.take_save_result();
        assert_eq!(*calls.lock(), [false, true]);
        assert!(app.deps.replica.cfg.lock().auto_start);
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
