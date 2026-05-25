//! About tab — version, release name, config path, manual update check.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::{runtime, update, AGENT_VERSION, RELEASE_NAME};

#[derive(Debug, Clone, Default)]
pub struct AboutState {
    pub last_check: Arc<Mutex<Option<CheckLine>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckLine {
    InFlight,
    Result(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AboutView {
    pub version: &'static str,
    pub release_name: &'static str,
    pub config_path: PathBuf,
    pub last_check: Option<CheckLine>,
}

impl AboutView {
    pub fn build(state: &AboutState, config_path: &Path) -> Self {
        Self {
            version: AGENT_VERSION,
            release_name: RELEASE_NAME,
            config_path: config_path.to_path_buf(),
            last_check: state.last_check.lock().clone(),
        }
    }
}

pub fn render(
    ui: &mut egui::Ui,
    view: &AboutView,
    state: &AboutState,
    tokio: &Handle,
    config_path: &Path,
) {
    ui.heading("About studio-worker");
    ui.add_space(4.0);

    egui::Grid::new("about_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Version");
            ui.monospace(view.version);
            ui.end_row();

            ui.label("Sentry release");
            ui.monospace(view.release_name);
            ui.end_row();

            ui.label("Config file");
            ui.monospace(view.config_path.to_string_lossy());
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let busy = matches!(view.last_check, Some(CheckLine::InFlight));
        if ui
            .add_enabled(!busy, egui::Button::new("Check for updates"))
            .clicked()
        {
            spawn_check(
                tokio.clone(),
                state.last_check.clone(),
                config_path.to_path_buf(),
            );
        }
        match &view.last_check {
            None => {}
            Some(CheckLine::InFlight) => {
                ui.spinner();
                ui.label("Checking the release feed\u{2026}");
            }
            Some(CheckLine::Result(line)) => {
                ui.label(line);
            }
        }
    });
}

fn spawn_check(tokio: Handle, slot: Arc<Mutex<Option<CheckLine>>>, config_path: PathBuf) {
    *slot.lock() = Some(CheckLine::InFlight);
    let path_str = config_path.to_string_lossy().to_string();
    tokio.spawn(async move {
        // Build a fresh CheckOutcome string through the same formatter
        // `studio-worker check-update` uses on the CLI so messages are
        // identical between surfaces.
        let outcome = run_check(Some(path_str.as_str())).await;
        let line = match outcome {
            Ok(o) => runtime::format_check_outcome(&o),
            Err(e) => format!("check failed: {e}"),
        };
        *slot.lock() = Some(CheckLine::Result(line));
    });
}

async fn run_check(config_path: Option<&str>) -> anyhow::Result<update::CheckOutcome> {
    use semver::Version;

    let (cfg, _) = crate::config::load(config_path)?;
    let current = Version::parse(AGENT_VERSION)?;
    let outcome = tokio::task::spawn_blocking(move || {
        update::check(&cfg.auto_update_feed, &current, cfg.auto_update_prerelease)
    })
    .await??;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_returns_static_version_strings() {
        let state = AboutState::default();
        let view = AboutView::build(&state, Path::new("/tmp/c.toml"));
        assert_eq!(view.version, AGENT_VERSION);
        assert_eq!(view.release_name, RELEASE_NAME);
        assert_eq!(view.config_path, PathBuf::from("/tmp/c.toml"));
        assert!(view.last_check.is_none());
    }

    #[test]
    fn build_surfaces_last_check_when_set() {
        let state = AboutState::default();
        *state.last_check.lock() = Some(CheckLine::Result("up to date".into()));
        let view = AboutView::build(&state, Path::new("/tmp/c.toml"));
        assert_eq!(
            view.last_check,
            Some(CheckLine::Result("up to date".into()))
        );
    }
}
