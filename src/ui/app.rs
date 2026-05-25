//! The eframe `App` impl.  Holds shared state (the same `Arc<Mutex<…>>`
//! handles the runtime loops use) and dispatches to per-tab renderers.

use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use eframe::egui;
use parking_lot::Mutex;

use crate::{
    config::SharedConfig,
    runtime::WorkerObservers,
    types::LogEntry,
};

use super::tab::Tab;

/// Everything `App` needs to render and act on the world.
pub struct AppDeps {
    pub cfg: SharedConfig,
    pub logs: Arc<Mutex<Vec<LogEntry>>>,
    pub busy: Arc<AtomicBool>,
    pub observers: WorkerObservers,
    pub stop: Arc<AtomicBool>,
    pub config_path: PathBuf,
}

pub struct App {
    deps: AppDeps,
    tab: Tab,
}

impl App {
    pub fn new(deps: AppDeps) -> Self {
        Self {
            deps,
            tab: Tab::default(),
        }
    }

    /// Shared by both the real `update` and the headless test harness so
    /// the layout is exercised in tests too.
    pub fn render(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
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

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Status => render_placeholder(ui, "Status"),
            Tab::Jobs => render_placeholder(ui, "Jobs"),
            Tab::Config => render_placeholder(ui, "Config"),
            Tab::Logs => render_placeholder(ui, "Logs"),
            Tab::About => render_placeholder(ui, "About"),
        });

        // The background loops mutate shared state asynchronously; ask
        // egui to repaint so updates surface without a user event.
        ctx.request_repaint_after(Duration::from_millis(250));
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

fn render_placeholder(ui: &mut egui::Ui, name: &str) {
    ui.heading(name);
    ui.add_space(8.0);
    ui.label(format!(
        "{name} tab — populated in a later phase of plans/native-ui.md."
    ));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::{config::Config, runtime::WorkerObservers};

    fn mock_deps() -> AppDeps {
        let cfg = crate::config::shared(Config::default());
        let logs = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let observers = WorkerObservers {
            current_job: Arc::new(Mutex::new(None)),
            recent_jobs: Arc::new(Mutex::new(VecDeque::new())),
            last_heartbeat: Arc::new(Mutex::new(None)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        AppDeps {
            cfg,
            logs,
            busy,
            observers,
            stop,
            config_path: PathBuf::from("/tmp/studio-worker-test.toml"),
        }
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
    fn render_does_not_panic_under_test_ctx() {
        let mut app = App::new(mock_deps());
        egui::__run_test_ctx(|ctx| {
            app.render(ctx);
        });
    }

    #[test]
    fn render_each_tab_does_not_panic() {
        for tab in Tab::ALL {
            let mut app = App::new(mock_deps());
            app.set_tab(tab);
            egui::__run_test_ctx(|ctx| {
                app.render(ctx);
            });
        }
    }
}
