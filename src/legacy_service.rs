//! Removes the headless daemon service older versions installed (`install-service`, and the
//! old `setup`).  Installed, the worker runs as the tray UI only; the tray UI calls [`remove`]
//! when it starts.  The removal deregisters and deletes the unit without stopping a running
//! legacy daemon: it keeps serving until it exits and never starts again.  See
//! `docs/runtime/daemon-and-tray.md#legacy-services`.
//!
//! The OS commands go through [`LegacyOps`] so the removal is unit-tested without them.
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, warn};

const TRACE_TARGET: &str = "studio_worker::legacy_service";

/// The `op` field of every event this module emits.
const OP: &str = "legacy_service";

#[cfg(target_os = "linux")]
const UNIT_NAME: &str = "minis-studio-worker.service";
#[cfg(target_os = "windows")]
const TASK_NAME: &str = "MinisStudioWorker";

/// Outcome of running a single OS command step (e.g. `systemctl
/// daemon-reload`, `launchctl unload`, `schtasks /Delete`).  Splits the
/// three observable states so callers can compose them into an overall
/// activation/deactivation success and so each one emits a distinct
/// structured tracing event instead of being silently swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Command spawned and exited with a zero status.
    Succeeded,
    /// Command spawned but exited non-zero (with the captured code if any).
    Failed { code: Option<i32> },
    /// Spawn itself failed — tool missing on PATH, permission denied, etc.
    SpawnFailed,
}

impl StepOutcome {
    pub fn is_success(self) -> bool {
        matches!(self, StepOutcome::Succeeded)
    }
}

/// Pure mapping from a `Command::status()` result onto [`StepOutcome`].
pub fn classify_status(status: std::io::Result<std::process::ExitStatus>) -> StepOutcome {
    match status {
        Ok(s) if s.success() => StepOutcome::Succeeded,
        Ok(s) => StepOutcome::Failed { code: s.code() },
        Err(_) => StepOutcome::SpawnFailed,
    }
}

/// Run a single command step and emit a structured tracing event for
/// the outcome.  Returns the classified [`StepOutcome`] so callers can
/// chain steps and short-circuit on failure.  Without this every
/// `RealOps::activate` / `deactivate` step would silently swallow
/// failures of systemctl / launchctl / schtasks.
fn run_step(op: &'static str, step: &'static str, mut cmd: Command) -> StepOutcome {
    let started = std::time::Instant::now();
    let status = cmd.status();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // Capture the spawn error's text before `classify_status` consumes the
    // result and drops it; a SpawnFailed otherwise loses its root cause.
    let spawn_error = match &status {
        Err(e) => Some(e.to_string()),
        Ok(_) => None,
    };
    let outcome = classify_status(status);
    match outcome {
        StepOutcome::Succeeded => {
            info!(
                target: TRACE_TARGET,
                op,
                step,
                elapsed_ms,
                "service step succeeded"
            );
        }
        StepOutcome::Failed { code } => {
            warn!(
                target: TRACE_TARGET,
                op,
                step,
                elapsed_ms,
                exit_code = code,
                "service step exited non-zero"
            );
        }
        StepOutcome::SpawnFailed => {
            warn!(
                target: TRACE_TARGET,
                op,
                step,
                elapsed_ms,
                error = spawn_error.as_deref().unwrap_or("unknown"),
                "service step could not be spawned (tool missing on PATH?)"
            );
        }
    }
    outcome
}

/// Where the legacy unit file lives on this OS, if it has one.
pub fn unit_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = directories::BaseDirs::new().map(|dirs| {
        dirs.config_dir()
            .join("systemd")
            .join("user")
            .join(UNIT_NAME)
    });
    #[cfg(target_os = "macos")]
    let path = std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join("gg.minis.studio-worker.plist")
    });
    #[cfg(target_os = "windows")]
    let path = std::env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("minis-studio-worker")
            .join("minis-studio-worker.task.xml")
    });
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let path = None;
    path
}

/// The OS side of the removal.
pub trait LegacyOps {
    /// Where the legacy unit file would be.
    fn unit_path(&self) -> Option<PathBuf>;
    /// The OS has the service registered even without its file (a Windows scheduled task).
    fn registered(&self) -> bool {
        false
    }
    /// Deregister the service without stopping it.
    fn deregister(&self) -> StepOutcome;
    /// Let the service manager forget the deleted file.
    fn reload(&self) -> StepOutcome {
        StepOutcome::Succeeded
    }
}

/// What [`remove_with`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// No legacy service is installed.
    NotInstalled,
    /// Deregistered and deleted.
    Removed,
    /// Found, but a step failed (each failure is logged).
    Incomplete,
}

/// Remove the legacy service through `ops`, logging every step.
pub fn remove_with<O: LegacyOps>(ops: &O) -> Removal {
    let file = ops.unit_path().filter(|path| path.exists());
    if file.is_none() && !ops.registered() {
        debug!(target: TRACE_TARGET, op = OP, "no legacy headless service installed");
        return Removal::NotInstalled;
    }
    info!(
        target: TRACE_TARGET,
        op = OP,
        unit_path = file.as_deref().map(|p| p.display().to_string()),
        "legacy headless service found; removing it (a running legacy daemon keeps serving \
         until it exits, and does not start again)"
    );
    let mut complete = ops.deregister().is_success();
    if let Some(file) = &file {
        complete &= delete_unit_file(file);
    }
    complete &= ops.reload().is_success();
    if complete {
        info!(target: TRACE_TARGET, op = OP, "legacy headless service removed");
        Removal::Removed
    } else {
        warn!(
            target: TRACE_TARGET,
            op = OP,
            "legacy headless service only partly removed; see the failed step above"
        );
        Removal::Incomplete
    }
}

fn delete_unit_file(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            info!(target: TRACE_TARGET, op = OP, unit_path = %path.display(), "legacy unit file deleted");
            true
        }
        Err(e) => {
            warn!(
                target: TRACE_TARGET,
                op = OP,
                unit_path = %path.display(),
                error = %e,
                "could not delete the legacy unit file"
            );
            false
        }
    }
}

/// Remove this machine's legacy service, if any.  Never fails: every problem is logged.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn remove() -> Removal {
    remove_with(&RealOps)
}

/// The real service managers.
pub struct RealOps;

impl LegacyOps for RealOps {
    fn unit_path(&self) -> Option<PathBuf> {
        unit_path()
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn registered(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            Command::new("schtasks")
                .args(["/Query", "/TN", TASK_NAME])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn deregister(&self) -> StepOutcome {
        #[cfg(target_os = "linux")]
        {
            let mut disable = Command::new("systemctl");
            disable.args(["--user", "disable", UNIT_NAME]);
            run_step(OP, "systemctl-disable", disable)
        }
        #[cfg(target_os = "windows")]
        {
            let mut delete = Command::new("schtasks");
            delete.args(["/Delete", "/TN", TASK_NAME, "/F"]);
            run_step(OP, "schtasks-delete", delete)
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            // A LaunchAgent is registered by its file alone; deleting it is the removal.
            StepOutcome::Succeeded
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn reload(&self) -> StepOutcome {
        #[cfg(target_os = "linux")]
        {
            let mut reload = Command::new("systemctl");
            reload.args(["--user", "daemon-reload"]);
            run_step(OP, "systemctl-daemon-reload", reload)
        }
        #[cfg(not(target_os = "linux"))]
        {
            StepOutcome::Succeeded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;
    use std::cell::Cell;
    use tempfile::tempdir;

    struct FakeOps {
        unit: Option<PathBuf>,
        registered: bool,
        deregister: StepOutcome,
        reload: StepOutcome,
        deregistered: Cell<u32>,
        reloaded: Cell<u32>,
    }

    impl FakeOps {
        fn new(unit: Option<PathBuf>) -> Self {
            Self {
                unit,
                registered: false,
                deregister: StepOutcome::Succeeded,
                reload: StepOutcome::Succeeded,
                deregistered: Cell::new(0),
                reloaded: Cell::new(0),
            }
        }
    }

    impl LegacyOps for FakeOps {
        fn unit_path(&self) -> Option<PathBuf> {
            self.unit.clone()
        }
        fn registered(&self) -> bool {
            self.registered
        }
        fn deregister(&self) -> StepOutcome {
            self.deregistered.set(self.deregistered.get() + 1);
            self.deregister
        }
        fn reload(&self) -> StepOutcome {
            self.reloaded.set(self.reloaded.get() + 1);
            self.reload
        }
    }

    fn installed_unit(dir: &Path) -> PathBuf {
        let unit = dir.join("minis-studio-worker.service");
        std::fs::write(&unit, "[Service]\nExecStart=/x/studio-worker run\n").unwrap();
        unit
    }

    #[test]
    fn nothing_installed_means_nothing_is_touched() {
        let dir = tempdir().unwrap();
        let ops = FakeOps::new(Some(dir.path().join("minis-studio-worker.service")));
        assert_eq!(remove_with(&ops), Removal::NotInstalled);
        assert_eq!(ops.deregistered.get(), 0);
        assert_eq!(ops.reloaded.get(), 0);
        assert_eq!(remove_with(&FakeOps::new(None)), Removal::NotInstalled);
    }

    #[test]
    fn an_installed_unit_is_deregistered_deleted_and_forgotten() {
        let dir = tempdir().unwrap();
        let unit = installed_unit(dir.path());
        let ops = FakeOps::new(Some(unit.clone()));
        let out = capture(move || assert_eq!(remove_with(&ops), Removal::Removed));
        assert!(!unit.exists(), "the unit file is deleted");
        assert!(out.contains("legacy headless service found"), "{out}");
        assert!(out.contains("legacy unit file deleted"), "{out}");
        assert!(out.contains("legacy headless service removed"), "{out}");
        assert!(
            out.contains("op=\"legacy_service\"") || out.contains("op=legacy_service"),
            "{out}"
        );
    }

    #[test]
    fn every_step_runs_once() {
        let dir = tempdir().unwrap();
        let ops = FakeOps::new(Some(installed_unit(dir.path())));
        remove_with(&ops);
        assert_eq!(ops.deregistered.get(), 1);
        assert_eq!(ops.reloaded.get(), 1);
    }

    #[test]
    fn a_registered_task_without_its_file_is_still_removed() {
        let dir = tempdir().unwrap();
        let mut ops = FakeOps::new(Some(dir.path().join("minis-studio-worker.task.xml")));
        ops.registered = true;
        assert_eq!(remove_with(&ops), Removal::Removed);
        assert_eq!(ops.deregistered.get(), 1);
    }

    #[test]
    fn a_failed_deregistration_still_deletes_the_file_and_warns() {
        let dir = tempdir().unwrap();
        let unit = installed_unit(dir.path());
        let mut ops = FakeOps::new(Some(unit.clone()));
        ops.deregister = StepOutcome::SpawnFailed;
        let out = capture(move || assert_eq!(remove_with(&ops), Removal::Incomplete));
        assert!(!unit.exists());
        assert!(
            out.contains("WARN") && out.contains("only partly removed"),
            "{out}"
        );
    }

    #[test]
    fn a_failed_reload_is_incomplete() {
        let dir = tempdir().unwrap();
        let mut ops = FakeOps::new(Some(installed_unit(dir.path())));
        ops.reload = StepOutcome::Failed { code: Some(1) };
        assert_eq!(remove_with(&ops), Removal::Incomplete);
    }

    #[test]
    fn a_unit_file_that_cannot_be_deleted_is_incomplete_and_named() {
        let dir = tempdir().unwrap();
        // A non-empty directory where the unit file should be: remove_file fails.
        let unit = dir.path().join("minis-studio-worker.service");
        std::fs::create_dir(&unit).unwrap();
        std::fs::write(unit.join("blocker"), b"x").unwrap();
        let ops = FakeOps::new(Some(unit));
        let out = capture(move || assert_eq!(remove_with(&ops), Removal::Incomplete));
        assert!(
            out.contains("could not delete the legacy unit file"),
            "{out}"
        );
        assert!(out.contains("minis-studio-worker.service"), "{out}");
    }

    #[test]
    fn the_unit_path_is_the_one_older_versions_wrote() {
        let path = unit_path().expect("CI hosts resolve a config dir");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            [
                "minis-studio-worker.service",
                "gg.minis.studio-worker.plist",
                "minis-studio-worker.task.xml"
            ]
            .contains(&name.as_str()),
            "{name}"
        );
    }

    #[test]
    fn classify_status_recognises_zero_exit_as_succeeded() {
        // Spawn the running test binary with `--list` (the cargo test
        // harness accepts it and exits 0).
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .status();
        assert_eq!(classify_status(status), StepOutcome::Succeeded);
    }

    #[test]
    fn classify_status_recognises_non_zero_exit_as_failed() {
        // The cargo test harness rejects an unknown long flag with a
        // non-zero exit.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--definitely-not-a-real-flag-zzzqx")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match classify_status(status) {
            StepOutcome::Failed { .. } => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_recognises_spawn_failure() {
        let status =
            std::process::Command::new("definitely-not-on-path-zzzqxq-studio-worker").status();
        assert_eq!(classify_status(status), StepOutcome::SpawnFailed);
    }

    #[test]
    fn run_step_spawn_failure_logs_underlying_io_error() {
        // A spawn failure can be ENOENT (tool missing) *or* a permission
        // error, a broken interpreter, etc.  The warn must carry the real
        // OS error so an operator isn't misled by the generic "missing on
        // PATH?" hint when the true cause is something else.
        let cmd = std::process::Command::new("definitely-not-on-path-zzzqxq-studio-worker");
        let logs = capture(move || {
            assert_eq!(run_step("activate", "smoke", cmd), StepOutcome::SpawnFailed);
        });
        assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
        assert!(
            logs.contains("could not be spawned"),
            "expected spawn-failure message, got: {logs}"
        );
        assert!(
            logs.contains("error="),
            "expected structured error field, got: {logs}"
        );
        // The concrete wording differs per OS — Unix renders "... (os
        // error 2)", Windows renders `error="program not found"` — so
        // assert the structured error field carries *some* underlying
        // detail rather than any one platform's phrasing.
        let lower = logs.to_lowercase();
        assert!(
            lower.contains("os error")
                || lower.contains("not found")
                || lower.contains("cannot find"),
            "expected the underlying io::Error text, got: {logs}"
        );
    }

    #[test]
    fn step_outcome_is_success_only_for_succeeded() {
        assert!(StepOutcome::Succeeded.is_success());
        assert!(!StepOutcome::Failed { code: Some(1) }.is_success());
        assert!(!StepOutcome::Failed { code: None }.is_success());
        assert!(!StepOutcome::SpawnFailed.is_success());
    }
}
