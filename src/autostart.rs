//! Autostart-on-login toggle.  Writes per-OS artefacts:
//!
//! - Linux: `~/.config/autostart/studio-worker-ui.desktop`
//! - macOS: `~/Library/LaunchAgents/gg.minis.studio-worker-ui.plist`
//! - Windows: a marker file under `%LOCALAPPDATA%\minis-studio-worker\`.
//!   The real HKCU `Software\Microsoft\Windows\CurrentVersion\Run`
//!   registry entry is the proper Windows mechanism but is not wired
//!   yet (the desktop UI does not ship for Windows); the marker only
//!   records intent and keeps `is_enabled()` truthful.
//!
//! Mirrors `service.rs`: every state change flows through the
//! path-injectable [`write_entry`] / [`remove_entry`] helpers, which
//! emit a structured `tracing` event for the outcome so a failed
//! toggle is never silently swallowed.  The helpers take an explicit
//! path so the disk-write round-trip is unit-tested against a tempdir
//! without mutating the process environment.
//!
//! Distinct from `service::install` because that owns the systemd /
//! launchd / Scheduled-Task path for the headless `run` subcommand;
//! autostart is for the tray UI on a desktop.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

/// Tracing target for autostart events.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::autostart=debug`.
const TRACE_TARGET: &str = "studio_worker::autostart";

pub const ENTRY_NAME: &str = "studio-worker-ui";

/// Pure-data render of the `.desktop` file body so tests don't touch
/// the filesystem.
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

#[cfg(target_os = "linux")]
fn autostart_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("autostart")
        .join(format!("{ENTRY_NAME}.desktop")))
}

#[cfg(target_os = "macos")]
fn autostart_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("gg.minis.{ENTRY_NAME}.plist")))
}

#[cfg(target_os = "windows")]
fn autostart_path() -> Result<PathBuf> {
    // Marker file mirrors the registry entry so callers can probe
    // `is_enabled()` without registry access (used by tests).
    let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow!("LOCALAPPDATA not set"))?;
    Ok(PathBuf::from(local)
        .join("minis-studio-worker")
        .join(format!("{ENTRY_NAME}.autostart")))
}

pub fn is_enabled() -> bool {
    is_enabled_at(autostart_path().ok().as_deref())
}

pub fn enable(exe: &Path) -> Result<()> {
    enable_at(&autostart_path()?, exe)
}

pub fn disable() -> Result<()> {
    disable_at(&autostart_path()?)
}

/// Path-injectable core of [`is_enabled`].  `None` (e.g. `HOME` /
/// `LOCALAPPDATA` unset, so the path can't be resolved) reads as
/// disabled rather than panicking.
fn is_enabled_at(path: Option<&Path>) -> bool {
    path.map(|p| p.exists()).unwrap_or(false)
}

/// Path-injectable core of [`enable`]: render the platform artefact
/// for `exe` and persist it at `path`.  Split out so the
/// render-then-write round-trip is unit-tested against a tempdir
/// without mutating the process `HOME` / `LOCALAPPDATA` (mirrors the
/// [`write_entry`] / [`remove_entry`] seam).
fn enable_at(path: &Path, exe: &Path) -> Result<()> {
    write_entry(path, &render_artefact(exe))
}

/// Path-injectable core of [`disable`]: remove the artefact at `path`
/// (idempotent).
fn disable_at(path: &Path) -> Result<()> {
    remove_entry(path)
}

/// Write the autostart artefact to `path`, emitting a structured
/// tracing event for the outcome.  Split from [`enable`] so the disk
/// write is unit-testable against a tempdir without mutating the
/// process `HOME` / `LOCALAPPDATA`.
fn write_entry(path: &Path, body: &str) -> Result<()> {
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

/// Remove the autostart artefact at `path` (idempotent), emitting a
/// structured tracing event for the outcome.  Split from [`disable`]
/// for the same testability reason as [`write_entry`].
fn remove_entry(path: &Path) -> Result<()> {
    if !path.exists() {
        info!(
            target: TRACE_TARGET,
            op = "disable",
            path = %path.display(),
            "autostart-on-login already disabled"
        );
        return Ok(());
    }
    let result = std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()));
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

fn render_artefact(exe: &Path) -> String {
    let exe_str = exe.to_string_lossy().to_string();
    if cfg!(target_os = "linux") {
        render_desktop_entry(&exe_str)
    } else if cfg!(target_os = "macos") {
        render_launch_agent(&exe_str)
    } else {
        // Windows marker — records intent and keeps the round-trip
        // test + `is_enabled()` honest.  The proper mechanism is an
        // HKCU `...\Run` registry entry, deferred until the desktop
        // UI ships for Windows (see module docs).
        format!("studio-worker-ui autostart enabled for {exe_str}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;
    use tempfile::tempdir;

    // -----------------------------------------------------------------
    // Disk-write round-trip + structured tracing.  The plan deferred
    // the "full disk-write round-trip" past v1; these helpers take an
    // explicit path so the success / failure / idempotent branches are
    // covered without mutating the process `HOME`, and `capture()`
    // proves each branch emits an operator-visible breadcrumb (a
    // failed toggle used to be silently swallowed by the caller).
    // -----------------------------------------------------------------

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
            // No file present; removal must still succeed.
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
        // A regular file where a directory is needed: `create_dir_all`
        // of the parent fails, so the write surfaces an error instead
        // of silently no-op-ing.
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

    // -----------------------------------------------------------------
    // Toggle round-trip via the path-injectable `*_at` seam.  Locks the
    // behaviour the public `enable` / `disable` / `is_enabled` wrappers
    // delegate to verbatim, against a tempdir — never touching the real
    // `HOME` / `LOCALAPPDATA` or mutating the process environment.
    // -----------------------------------------------------------------

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
        // `autostart_path()` returns `Err` when `HOME` / `LOCALAPPDATA`
        // is unset; the wrapper maps that to `None`, which must read as
        // disabled rather than panic.
        assert!(!is_enabled_at(None));
    }

    #[test]
    fn render_artefact_selects_the_platform_template() {
        let exe = Path::new("/opt/studio-worker/studio-worker");
        let body = render_artefact(exe);
        #[cfg(target_os = "linux")]
        assert_eq!(
            body,
            render_desktop_entry("/opt/studio-worker/studio-worker")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            body,
            render_launch_agent("/opt/studio-worker/studio-worker")
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert!(body.contains("autostart enabled for /opt/studio-worker/studio-worker"));
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
        // The public wrapper must agree with the resolved path's state.
        assert_eq!(is_enabled(), path.exists());
    }
}
