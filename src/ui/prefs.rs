//! The tray UI's own preferences: theme, reduce motion, notifications.
//!
//! They belong to the window, not the worker, so they live in
//! `<config dir>/ui.toml` rather than the daemon's config, and are applied
//! and saved the moment the operator changes them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::notifier::NotificationPrefs;
use super::theme::ThemeChoice;

const TRACE_TARGET: &str = "studio_worker::ui";

/// The preferences file's name, next to `config.toml`.
pub const PREFS_FILE_NAME: &str = "ui.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub theme: ThemeChoice,
    /// Hold the running glow steady.
    pub reduce_motion: bool,
    pub notify_on_completion: bool,
    pub notify_on_failure: bool,
}

impl UiPrefs {
    pub fn notifications(&self) -> NotificationPrefs {
        NotificationPrefs {
            on_completion: self.notify_on_completion,
            on_failure: self.notify_on_failure,
        }
    }
}

/// The preferences file for the config at `config_path`.
pub fn path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PREFS_FILE_NAME)
}

/// Read the preferences at `path`; a missing file means the defaults, an
/// unreadable one is logged and means the defaults.
pub fn load(path: &Path) -> UiPrefs {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return UiPrefs::default(),
        Err(e) => {
            warn_unreadable(path, &e.to_string());
            return UiPrefs::default();
        }
    };
    toml::from_str(&raw).unwrap_or_else(|e| {
        warn_unreadable(path, &e.to_string());
        UiPrefs::default()
    })
}

fn warn_unreadable(path: &Path, error: &str) {
    tracing::warn!(
        target: TRACE_TARGET,
        op = "prefs",
        path = %path.display(),
        error = %error,
        "ui preferences unreadable; using the defaults"
    );
}

/// Write `prefs` to `path` atomically; a failure is logged and returned.
pub fn save(path: &Path, prefs: &UiPrefs) -> Result<(), String> {
    let outcome = (|| -> anyhow::Result<()> {
        let body = toml::to_string(prefs)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            tracing::info!(
                target: TRACE_TARGET,
                op = "prefs",
                path = %path.display(),
                theme = ?prefs.theme,
                reduce_motion = prefs.reduce_motion,
                "ui preferences saved"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "prefs",
                path = %path.display(),
                error = %e,
                "ui preferences not saved"
            );
            Err(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_sits_next_to_the_config() {
        assert_eq!(
            path_for(Path::new("/etc/sw/config.toml")),
            PathBuf::from("/etc/sw/ui.toml")
        );
    }

    #[test]
    fn the_defaults_are_dark_moving_and_quiet() {
        let prefs = UiPrefs::default();
        assert_eq!(prefs.theme, ThemeChoice::Dark);
        assert!(!prefs.reduce_motion);
        assert_eq!(prefs.notifications(), NotificationPrefs::default());
    }

    #[test]
    fn saved_preferences_load_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFS_FILE_NAME);
        let prefs = UiPrefs {
            theme: ThemeChoice::Light,
            reduce_motion: true,
            notify_on_completion: false,
            notify_on_failure: true,
        };
        save(&path, &prefs).unwrap();
        assert_eq!(load(&path), prefs);
        assert!(load(&path).notifications().on_failure);
    }

    #[test]
    fn a_missing_file_means_the_defaults_and_a_partial_one_fills_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFS_FILE_NAME);
        assert_eq!(load(&path), UiPrefs::default());
        std::fs::write(&path, "theme = \"system\"\n").unwrap();
        assert_eq!(load(&path).theme, ThemeChoice::System);
    }

    #[test]
    fn a_broken_file_is_logged_and_means_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFS_FILE_NAME);
        std::fs::write(&path, "theme = 12").unwrap();
        let logs = crate::test_support::capture({
            let path = path.clone();
            move || assert_eq!(load(&path), UiPrefs::default())
        });
        assert!(logs.contains("op=\"prefs\""), "{logs}");
        assert!(logs.contains("unreadable"), "{logs}");

        // A directory where the file should be cannot be read either.
        let dir_path = dir.path().join("as-dir");
        std::fs::create_dir(&dir_path).unwrap();
        let logs = crate::test_support::capture(move || {
            assert_eq!(load(&dir_path), UiPrefs::default());
        });
        assert!(logs.contains("unreadable"), "{logs}");
    }

    #[test]
    fn a_failed_save_is_logged_and_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join(PREFS_FILE_NAME);
        let logs = crate::test_support::capture(move || {
            assert!(save(&path, &UiPrefs::default()).is_err());
        });
        assert!(logs.contains("not saved"), "{logs}");
    }
}
