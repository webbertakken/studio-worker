//! One tray UI per config directory.
//!
//! `ui` holds `<config dir>/ui.lock` exclusively for its whole life; the OS
//! releases it when the process exits, however it exits.  A second `ui` for
//! the same config finds the lock taken, leaves a raise request
//! (`ui.raise`) for the running one and exits, so a config never gets two
//! tray icons.  The running UI watches for the request and shows its window.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TRACE_TARGET: &str = "studio_worker::ui";

/// The UI lock's file name, next to `config.toml`.
pub const LOCK_FILE_NAME: &str = "ui.lock";
/// A second launch's request to show the running UI's window.
pub const RAISE_FILE_NAME: &str = "ui.raise";
/// How often the running UI looks for a raise request.
pub const RAISE_POLL: Duration = Duration::from_millis(250);
/// How long a UI restarting itself for the display waits for the lock its
/// predecessor still holds: attempts × pause = 5 s.
pub const RESTART_ATTEMPTS: u32 = 25;
pub const RESTART_PAUSE: Duration = Duration::from_millis(200);

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// The UI lock for the config at `config_path`.
pub fn lock_path_for(config_path: &Path) -> PathBuf {
    config_dir(config_path).join(LOCK_FILE_NAME)
}

/// The raise request for the config at `config_path`.
pub fn raise_path_for(config_path: &Path) -> PathBuf {
    config_dir(config_path).join(RAISE_FILE_NAME)
}

/// The held UI lock; dropping it releases the lock.
#[derive(Debug)]
pub struct UiLock {
    _file: File,
}

/// The outcome of [`acquire`].
#[derive(Debug)]
pub enum Instance {
    /// This process is the tray UI for the config.
    Primary(UiLock),
    /// Another tray UI holds the lock.
    Secondary,
}

/// Take the UI lock for the config at `config_path`, trying `attempts`
/// times `pause` apart.  A stale raise request is dropped once the lock is
/// ours, so it cannot pop the window open later.
pub fn acquire(config_path: &Path, attempts: u32, pause: Duration) -> std::io::Result<Instance> {
    let path = lock_path_for(config_path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    for attempt in 1..=attempts.max(1) {
        match file.try_lock() {
            Ok(()) => {
                take_raise_request(config_path);
                tracing::info!(
                    target: TRACE_TARGET,
                    op = "single_instance",
                    path = %path.display(),
                    pid = std::process::id(),
                    "ui lock taken"
                );
                return Ok(Instance::Primary(UiLock { _file: file }));
            }
            Err(TryLockError::WouldBlock) if attempt < attempts => std::thread::sleep(pause),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(Instance::Secondary)
}

/// Ask the running tray UI to show its window, and say so.
pub fn hand_over(config_path: &Path) -> std::io::Result<()> {
    let path = raise_path_for(config_path);
    let outcome = std::fs::write(&path, format!("{}\n", std::process::id()));
    match &outcome {
        Ok(()) => tracing::info!(
            target: TRACE_TARGET,
            op = "single_instance",
            raise = %path.display(),
            "another tray UI is running for this config; asked it to show its window and exiting"
        ),
        Err(e) => tracing::warn!(
            target: TRACE_TARGET,
            op = "single_instance",
            raise = %path.display(),
            error = %e,
            "another tray UI is running for this config; could not ask it to show its window"
        ),
    }
    outcome
}

/// Consume a pending raise request; answers whether there was one.
pub fn take_raise_request(config_path: &Path) -> bool {
    std::fs::remove_file(raise_path_for(config_path)).is_ok()
}

/// Until `stop`, consume raise requests and call `raise` for each, when it
/// can (`raise` answers whether the window could be reached).
// The loop only sequences `take_raise_request` (unit-tested) with sleeps.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn watch(config_path: PathBuf, stop: Arc<AtomicBool>, raise: impl Fn() -> bool) {
    while !stop.load(Ordering::SeqCst) {
        if raise_path_for(&config_path).exists() && raise() {
            take_raise_request(&config_path);
            tracing::info!(
                target: TRACE_TARGET,
                op = "raise",
                "a second launch asked for the window; showing it"
            );
        }
        std::thread::sleep(RAISE_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(1);

    #[test]
    fn the_lock_and_the_raise_request_sit_next_to_the_config() {
        let config = Path::new("/etc/sw/config.toml");
        assert_eq!(lock_path_for(config), PathBuf::from("/etc/sw/ui.lock"));
        assert_eq!(raise_path_for(config), PathBuf::from("/etc/sw/ui.raise"));
    }

    #[test]
    fn a_second_ui_for_the_same_config_is_secondary_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("nested").join("config.toml");

        let Instance::Primary(first) = acquire(&config, 1, FAST).unwrap() else {
            panic!("the first UI must get the lock");
        };
        assert!(matches!(
            acquire(&config, 2, FAST).unwrap(),
            Instance::Secondary
        ));

        drop(first);
        assert!(matches!(
            acquire(&config, 1, FAST).unwrap(),
            Instance::Primary(_)
        ));
    }

    #[test]
    fn uis_for_different_configs_run_side_by_side() {
        let dir = tempfile::tempdir().unwrap();
        let _a = acquire(&dir.path().join("a").join("config.toml"), 1, FAST).unwrap();
        assert!(matches!(
            acquire(&dir.path().join("b").join("config.toml"), 1, FAST).unwrap(),
            Instance::Primary(_)
        ));
    }

    #[test]
    fn a_hand_over_leaves_one_raise_request_and_logs_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let logs = crate::test_support::capture({
            let config = config.clone();
            move || hand_over(&config).unwrap()
        });
        assert!(logs.contains("op=\"single_instance\""), "{logs}");
        assert!(logs.contains("asked it to show its window"), "{logs}");
        assert!(take_raise_request(&config));
        assert!(!take_raise_request(&config), "a request is consumed once");
    }

    #[test]
    fn a_hand_over_that_cannot_write_is_logged_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("missing-dir").join("config.toml");
        let logs = crate::test_support::capture(move || {
            assert!(hand_over(&config).is_err());
        });
        assert!(logs.contains("could not ask it"), "{logs}");
    }

    #[test]
    fn taking_the_lock_drops_a_stale_raise_request() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(raise_path_for(&config), "1\n").unwrap();
        let _lock = acquire(&config, 1, FAST).unwrap();
        assert!(!raise_path_for(&config).exists());
    }
}
