//! Every way the tray UI ends leaves a line in `ui.log`: a closed window, a Quit, a hand-over to
//! the tray UI already running, a signal, a panic. The daemon relaunches a tray that vanished
//! unless the user quit it (`tray_watchdog`); the tray clears the user's quit marker when it starts.

use std::path::Path;

const TRACE_TARGET: &str = "studio_worker::ui";

/// Log that the tray UI ends, and why.
pub fn log_exit(reason: &str) {
    tracing::info!(target: TRACE_TARGET, op = "exit", reason, "tray UI exiting");
}

/// The tray UI runs again: forget that the user quit it.
pub fn clear_quit_marker(config_path: &Path) {
    let marker = crate::tray_watchdog::quit_marker_path(config_path);
    match std::fs::remove_file(&marker) {
        Ok(()) => tracing::info!(
            target: TRACE_TARGET,
            op = "startup",
            marker = %marker.display(),
            "cleared the quit marker; the daemon relaunches this tray UI if it vanishes"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            target: TRACE_TARGET,
            op = "startup",
            marker = %marker.display(),
            error = %e,
            "could not clear the quit marker; the daemon will not relaunch this tray UI"
        ),
    }
}

/// Log a panic before the previous hook (Sentry's, the default message) runs.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn install_panic_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        tracing::error!(
            target: TRACE_TARGET,
            op = "exit",
            reason = "panic",
            %location,
            %message,
            thread = std::thread::current().name().unwrap_or("unnamed"),
            "tray UI panicked"
        );
        previous(info);
    }));
}

/// Log the signal that ends the tray UI (logout, `kill`), then exit as the signal would.
#[cfg(unix)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn spawn_signal_log(tokio: &tokio::runtime::Handle) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio.spawn(async {
        let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "exit",
                "could not listen for signals; a signal ends the tray UI without a log line"
            );
            return;
        };
        let (name, code) = tokio::select! {
            _ = term.recv() => ("SIGTERM", 15),
            _ = hup.recv() => ("SIGHUP", 1),
            _ = int.recv() => ("SIGINT", 2),
        };
        log_exit(name);
        std::process::exit(128 + code);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn an_exit_says_why() {
        let logs = capture(|| log_exit("window closed"));
        assert!(logs.contains("op=\"exit\""), "{logs}");
        assert!(logs.contains("reason=\"window closed\""), "{logs}");
    }

    #[test]
    fn starting_clears_the_user_s_quit_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(dir.path().join("ui.quit"), b"").unwrap();
        let in_capture = config.clone();
        let logs = capture(move || clear_quit_marker(&in_capture));
        assert!(!dir.path().join("ui.quit").exists());
        assert!(logs.contains("cleared the quit marker"), "{logs}");
    }

    #[test]
    fn starting_without_a_quit_marker_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let logs = capture(move || clear_quit_marker(&config));
        assert!(!logs.contains("quit marker"), "{logs}");
    }
}
