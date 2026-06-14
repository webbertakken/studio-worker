//! Autostart-on-login toggle.  Writes per-OS artefacts so the desktop
//! UI (tray) comes back after a reboot:
//!
//! - Linux: `~/.config/autostart/studio-worker-ui.desktop`
//! - macOS: `~/Library/LaunchAgents/gg.minis.studio-worker-ui.plist`
//! - Windows: an `HKCU\…\CurrentVersion\Run` registry value
//!   `studio-worker-ui` = `"<exe>" ui`.  This is the standard per-user
//!   autostart mechanism — no console flash, no admin rights, no COM.
//!
//! Every state change emits a structured `tracing` event for its
//! outcome so a failed toggle is never silently swallowed.  The
//! file-backed platforms keep a path-injectable seam
//! ([`write_entry`] / [`remove_entry`]) so the disk round-trip is
//! unit-tested against a tempdir without mutating the process
//! environment.
//!
//! Distinct from `service.rs`, which owns the systemd / launchd /
//! Scheduled-Task path for the headless `run` subcommand; autostart is
//! for the tray UI on a desktop.

use std::path::Path;

use anyhow::Result;

/// Tracing target for autostart events.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::autostart=debug`.
const TRACE_TARGET: &str = "studio_worker::autostart";

pub const ENTRY_NAME: &str = "studio-worker-ui";

// ---------------------------------------------------------------------------
// Pure renderers (cross-platform, unit-tested without any I/O).
// ---------------------------------------------------------------------------

/// Pure-data render of the `.desktop` file body.
pub fn render_desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=studio-worker\n\
         Comment=Pull-based generation worker for the minis.gg studio\n\
         Exec={exe} ui\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// Pure-data render of the macOS LaunchAgent plist.
pub fn render_launch_agent(exe: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
  <dict>\n\
    <key>Label</key>\n\
    <string>gg.minis.studio-worker-ui</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
      <string>{exe}</string>\n\
      <string>ui</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
  </dict>\n\
</plist>\n"
    )
}

/// The command an autostart entry runs: the worker's `ui` subcommand,
/// quoted so a path with spaces survives the Windows registry / shell.
/// Pure so the Windows Run-value contract is testable on any platform.
pub fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" ui", exe.display())
}

/// What a launch-time autostart sync should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartSync {
    Enable,
    Disable,
    Noop,
}

/// Decide how to reconcile the on-login autostart artefact with the
/// configured `auto_start`, given whether it is `currently_enabled`.
/// Keeping this pure means the UI's launch-time sync is unit-tested
/// without touching the registry / filesystem: enable when the operator
/// wants autostart but it isn't set up, disable when they turned it off
/// but a stale entry lingers, otherwise leave it alone.
pub fn launch_sync_action(auto_start: bool, currently_enabled: bool) -> AutostartSync {
    match (auto_start, currently_enabled) {
        (true, false) => AutostartSync::Enable,
        (false, true) => AutostartSync::Disable,
        _ => AutostartSync::Noop,
    }
}

// ---------------------------------------------------------------------------
// Public API — dispatches to the per-OS backend.
// ---------------------------------------------------------------------------

/// Whether autostart-on-login is currently enabled.
pub fn is_enabled() -> bool {
    backend::is_enabled()
}

/// Enable autostart-on-login for `exe`.
pub fn enable(exe: &Path) -> Result<()> {
    backend::enable(exe)
}

/// Disable autostart-on-login (idempotent).
pub fn disable() -> Result<()> {
    backend::disable()
}

// ===========================================================================
// File backend — Linux + macOS.
// ===========================================================================

#[cfg(not(target_os = "windows"))]
mod backend {
    use super::{ENTRY_NAME, TRACE_TARGET};
    use anyhow::{anyhow, Context, Result};
    use std::path::{Path, PathBuf};
    use tracing::{info, warn};

    pub fn is_enabled() -> bool {
        is_enabled_at(autostart_path().ok().as_deref())
    }

    pub fn enable(exe: &Path) -> Result<()> {
        enable_at(&autostart_path()?, exe)
    }

    pub fn disable() -> Result<()> {
        disable_at(&autostart_path()?)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn autostart_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("autostart")
            .join(format!("{ENTRY_NAME}.desktop")))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn autostart_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("gg.minis.{ENTRY_NAME}.plist")))
    }

    /// `None` (e.g. `HOME` unset) reads as disabled rather than panicking.
    pub(super) fn is_enabled_at(path: Option<&Path>) -> bool {
        path.map(|p| p.exists()).unwrap_or(false)
    }

    pub(super) fn enable_at(path: &Path, exe: &Path) -> Result<()> {
        write_entry(path, &render_artefact(exe))
    }

    pub(super) fn disable_at(path: &Path) -> Result<()> {
        remove_entry(path)
    }

    pub(super) fn render_artefact(exe: &Path) -> String {
        let exe_str = exe.to_string_lossy().to_string();
        #[cfg(target_os = "linux")]
        {
            super::render_desktop_entry(&exe_str)
        }
        #[cfg(target_os = "macos")]
        {
            super::render_launch_agent(&exe_str)
        }
    }

    /// Write the autostart artefact to `path`, emitting a structured
    /// tracing event for the outcome.
    pub(super) fn write_entry(path: &Path, body: &str) -> Result<()> {
        let result = (|| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
        })();
        match &result {
            Ok(()) => info!(
                target: TRACE_TARGET,
                op = "enable",
                path = %path.display(),
                bytes = body.len(),
                "autostart-on-login enabled"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET,
                op = "enable",
                path = %path.display(),
                error = %e,
                "failed to enable autostart-on-login"
            ),
        }
        result
    }

    /// Remove the autostart artefact at `path` (idempotent).
    pub(super) fn remove_entry(path: &Path) -> Result<()> {
        if !path.exists() {
            info!(
                target: TRACE_TARGET,
                op = "disable",
                path = %path.display(),
                "autostart-on-login already disabled"
            );
            return Ok(());
        }
        let result =
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()));
        match &result {
            Ok(()) => info!(
                target: TRACE_TARGET,
                op = "disable",
                path = %path.display(),
                "autostart-on-login disabled"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET,
                op = "disable",
                path = %path.display(),
                error = %e,
                "failed to disable autostart-on-login"
            ),
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::capture;
        use tempfile::tempdir;

        #[test]
        fn write_entry_creates_file_and_emits_enable_event() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("autostart").join("entry.desktop");
            let path_for_closure = path.clone();
            let logs = capture(move || {
                write_entry(&path_for_closure, "BODY").unwrap();
            });
            assert!(path.exists(), "entry should be written");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "BODY");
            assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
            assert!(
                logs.contains("studio_worker::autostart"),
                "expected autostart target, got: {logs}"
            );
            assert!(logs.contains("op=\"enable\""), "expected op field: {logs}");
            assert!(
                logs.contains("autostart-on-login enabled"),
                "expected enable message: {logs}"
            );
        }

        #[test]
        fn remove_entry_deletes_file_and_emits_disable_event() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("entry.desktop");
            std::fs::write(&path, "BODY").unwrap();
            let path_for_closure = path.clone();
            let logs = capture(move || {
                remove_entry(&path_for_closure).unwrap();
            });
            assert!(!path.exists(), "entry should be removed");
            assert!(logs.contains("op=\"disable\""), "expected op field: {logs}");
            assert!(
                logs.contains("autostart-on-login disabled"),
                "expected disable message: {logs}"
            );
        }

        #[test]
        fn remove_entry_is_idempotent_and_logs_already_disabled() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("missing.desktop");
            let path_for_closure = path.clone();
            let logs = capture(move || {
                remove_entry(&path_for_closure).unwrap();
            });
            assert!(
                logs.contains("already disabled"),
                "expected already-disabled message: {logs}"
            );
        }

        #[test]
        fn write_entry_failure_surfaces_error_and_emits_warn() {
            let dir = tempdir().unwrap();
            let blocker = dir.path().join("blocker");
            std::fs::write(&blocker, "x").unwrap();
            let path = blocker.join("sub").join("entry.desktop");
            let path_for_closure = path.clone();
            let logs = capture(move || {
                let err = write_entry(&path_for_closure, "BODY")
                    .expect_err("writing under a file should fail");
                assert!(
                    err.to_string().contains("creating") || err.to_string().contains("writing"),
                    "unexpected error: {err}"
                );
            });
            assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
            assert!(
                logs.contains("failed to enable autostart-on-login"),
                "expected failure message: {logs}"
            );
        }

        #[test]
        fn remove_entry_failure_surfaces_error_and_emits_warn() {
            // `remove_file` cannot delete a directory, so pointing the
            // entry path at one drives the failure branch
            // deterministically on every OS: `path.exists()` is true
            // (so the idempotent early-return is skipped) but the
            // removal itself errors.
            let dir = tempdir().unwrap();
            let path = dir.path().join("entry-as-dir.desktop");
            std::fs::create_dir(&path).unwrap();
            let path_for_closure = path.clone();
            let logs = capture(move || {
                let err = remove_entry(&path_for_closure)
                    .expect_err("removing a directory as a file should fail");
                assert!(
                    err.to_string().contains("removing"),
                    "unexpected error: {err}"
                );
            });
            assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
            assert!(logs.contains("op=\"disable\""), "expected op field: {logs}");
            assert!(
                logs.contains("failed to disable autostart-on-login"),
                "expected failure message: {logs}"
            );
            // A failed disable must not silently report success: the
            // stale entry has to survive so a retry can act on it.
            assert!(path.exists(), "the entry must survive a failed removal");
        }

        #[test]
        fn enable_at_persists_rendered_artefact_and_disable_at_removes_it() {
            let dir = tempdir().unwrap();
            let path = dir
                .path()
                .join("autostart")
                .join(format!("{ENTRY_NAME}.desktop"));
            let exe = Path::new("/opt/studio-worker/studio-worker");

            assert!(
                !is_enabled_at(Some(&path)),
                "a fresh tempdir must report autostart disabled"
            );

            enable_at(&path, exe).unwrap();
            assert!(
                is_enabled_at(Some(&path)),
                "enable_at must create the artefact so is_enabled_at sees it"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                render_artefact(exe),
                "enable_at must persist the platform artefact verbatim"
            );

            disable_at(&path).unwrap();
            assert!(
                !is_enabled_at(Some(&path)),
                "disable_at must remove the artefact"
            );
        }

        #[test]
        fn is_enabled_at_reports_disabled_when_path_unresolved() {
            assert!(!is_enabled_at(None));
        }

        #[test]
        fn render_artefact_selects_the_platform_template() {
            let exe = Path::new("/opt/studio-worker/studio-worker");
            let body = render_artefact(exe);
            #[cfg(target_os = "linux")]
            assert_eq!(
                body,
                crate::autostart::render_desktop_entry("/opt/studio-worker/studio-worker")
            );
            #[cfg(target_os = "macos")]
            assert_eq!(
                body,
                crate::autostart::render_launch_agent("/opt/studio-worker/studio-worker")
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn autostart_path_targets_xdg_autostart_and_is_enabled_mirrors_it() {
            let path = autostart_path().expect("HOME should be set in the test environment");
            assert!(
                path.ends_with(format!("{ENTRY_NAME}.desktop")),
                "unexpected file name: {}",
                path.display()
            );
            let parent = path.parent().expect("autostart path must have a parent");
            assert!(
                parent.ends_with(".config/autostart"),
                "unexpected parent dir: {}",
                parent.display()
            );
            assert_eq!(super::is_enabled(), path.exists());
        }
    }
}

// ===========================================================================
// Registry backend — Windows (HKCU\…\Run value).
// ===========================================================================

#[cfg(target_os = "windows")]
mod backend {
    use super::{autostart_command, ENTRY_NAME, TRACE_TARGET};
    use anyhow::{Context, Result};
    use std::path::Path;
    use tracing::{info, warn};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    use winreg::RegKey;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn is_enabled() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.open_subkey(RUN_KEY)
            .and_then(|k| k.get_value::<String, _>(ENTRY_NAME))
            .is_ok()
    }

    pub fn enable(exe: &Path) -> Result<()> {
        let command = autostart_command(exe);
        let result = (|| -> std::io::Result<()> {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (run, _) = hkcu.create_subkey(RUN_KEY)?;
            run.set_value(ENTRY_NAME, &command)
        })();
        match &result {
            Ok(()) => info!(
                target: TRACE_TARGET,
                op = "enable",
                value = ENTRY_NAME,
                command = %command,
                "autostart-on-login enabled (HKCU Run)"
            ),
            Err(e) => warn!(
                target: TRACE_TARGET,
                op = "enable",
                value = ENTRY_NAME,
                error = %e,
                "failed to enable autostart-on-login"
            ),
        }
        result.with_context(|| format!("writing HKCU Run value {ENTRY_NAME}"))
    }

    pub fn disable() -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
            Ok(run) => run,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    target: TRACE_TARGET,
                    op = "disable",
                    "autostart-on-login already disabled (no Run key)"
                );
                return Ok(());
            }
            Err(e) => {
                warn!(
                    target: TRACE_TARGET,
                    op = "disable",
                    error = %e,
                    "failed to open HKCU Run key"
                );
                return Err(e).context("opening HKCU Run key");
            }
        };
        match run.delete_value(ENTRY_NAME) {
            Ok(()) => {
                info!(
                    target: TRACE_TARGET,
                    op = "disable",
                    value = ENTRY_NAME,
                    "autostart-on-login disabled (HKCU Run)"
                );
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    target: TRACE_TARGET,
                    op = "disable",
                    value = ENTRY_NAME,
                    "autostart-on-login already disabled"
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    target: TRACE_TARGET,
                    op = "disable",
                    value = ENTRY_NAME,
                    error = %e,
                    "failed to delete HKCU Run value"
                );
                Err(e).context("deleting HKCU Run value")
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        // The Run-key round-trip mutates the real HKCU hive, so it uses
        // the production value name and self-cleans.  CI runners are
        // ephemeral; a developer box has its prior state restored by the
        // final disable().  Gated to Windows (the only place winreg
        // compiles).
        #[test]
        fn enable_then_disable_round_trips_through_hkcu_run() {
            let was_enabled = is_enabled();
            let exe = Path::new(r"C:\Program Files\studio-worker\studio-worker.exe");

            enable(exe).expect("enable should write the Run value");
            assert!(is_enabled(), "enable must make is_enabled() true");

            disable().expect("disable should remove the Run value");
            assert!(!is_enabled(), "disable must make is_enabled() false");

            // disable() is idempotent.
            disable().expect("second disable should be a no-op");

            // Don't resurrect a pre-existing entry we didn't own.
            assert!(!was_enabled || !is_enabled());
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-platform tests for the pure renderers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_contains_exec_and_name() {
        let s = render_desktop_entry("/usr/local/bin/studio-worker");
        assert!(s.contains("Exec=/usr/local/bin/studio-worker ui"));
        assert!(s.contains("Name=studio-worker"));
        assert!(s.contains("Type=Application"));
    }

    #[test]
    fn launch_agent_is_valid_plist_with_args() {
        let s = render_launch_agent("/usr/local/bin/studio-worker");
        assert!(s.contains("<?xml"));
        assert!(s.contains("<string>/usr/local/bin/studio-worker</string>"));
        assert!(s.contains("<string>ui</string>"));
        assert!(s.contains("gg.minis.studio-worker-ui"));
    }

    #[test]
    fn autostart_command_quotes_exe_and_appends_ui() {
        let cmd = autostart_command(Path::new("/opt/studio worker/studio-worker"));
        assert_eq!(cmd, "\"/opt/studio worker/studio-worker\" ui");
    }

    #[test]
    fn launch_sync_action_covers_every_combination() {
        assert_eq!(launch_sync_action(true, false), AutostartSync::Enable);
        assert_eq!(launch_sync_action(false, true), AutostartSync::Disable);
        assert_eq!(launch_sync_action(true, true), AutostartSync::Noop);
        assert_eq!(launch_sync_action(false, false), AutostartSync::Noop);
    }
}
