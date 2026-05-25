//! OS service install/uninstall.  Linux: systemd --user.  macOS: launchd
//! plist template (written but not loaded — operator runs `launchctl load`).
//! Windows: schtasks template (written but not registered — operator runs
//! the printed command).
//!
//! All system side-effects (Command::status, fs writes) flow through the
//! `ServiceOps` trait so the public install/uninstall functions can be
//! unit-tested without touching the real OS.
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

const TRACE_TARGET: &str = "studio_worker::service";

/// Outcome of running a single OS command step (e.g. `systemctl
/// daemon-reload`, `launchctl unload`, `schtasks /Delete`).  Splits the
/// three observable states so callers can compose them into an overall
/// activation/deactivation success and so each one emits a distinct
/// structured tracing event instead of being silently swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    /// Command spawned and exited with a zero status.
    Succeeded,
    /// Command spawned but exited non-zero (with the captured code if any).
    Failed { code: Option<i32> },
    /// Spawn itself failed — tool missing on PATH, permission denied, etc.
    SpawnFailed,
}

impl StepOutcome {
    pub(crate) fn is_success(self) -> bool {
        matches!(self, StepOutcome::Succeeded)
    }
}

/// Pure mapping from a `Command::status()` result onto [`StepOutcome`].
pub(crate) fn classify_status(status: std::io::Result<std::process::ExitStatus>) -> StepOutcome {
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
                "service step could not be spawned (tool missing on PATH?)"
            );
        }
    }
    outcome
}

#[cfg(target_os = "linux")]
const SERVICE_FILENAME: &str = "minis-studio-worker.service";
#[cfg(target_os = "macos")]
const SERVICE_FILENAME: &str = "gg.minis.studio-worker.plist";
#[cfg(target_os = "windows")]
const SERVICE_FILENAME: &str = "minis-studio-worker.task.xml";

fn binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("resolving current executable path")
}

#[cfg(target_os = "linux")]
fn default_unit_dir() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().ok_or_else(|| anyhow!("cannot resolve user dirs"))?;
    let path = dirs.config_dir().join("systemd").join("user");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn default_unit_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home).join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(target_os = "windows")]
fn default_unit_dir() -> Result<PathBuf> {
    let app_data = std::env::var("APPDATA").context("APPDATA not set")?;
    let path = PathBuf::from(app_data).join("minis-studio-worker");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Abstraction over the side-effecting parts of install/uninstall so the
/// install logic itself is fully unit-testable.
pub trait ServiceOps {
    fn unit_dir(&self) -> Result<PathBuf>;
    fn binary_path(&self) -> Result<PathBuf>;
    /// Activate the unit (systemctl --user enable / launchctl load /
    /// schtasks /Create).  Implementations return false if the platform
    /// tool isn't available so install() can still succeed (file
    /// written, manual activation instructions printed).
    fn activate(&self, _unit_path: &Path) -> bool {
        false
    }
    fn deactivate(&self, _unit_path: &Path) {}
}

/// Real, system-touching implementation used by the CLI.
pub struct RealOps;

impl ServiceOps for RealOps {
    fn unit_dir(&self) -> Result<PathBuf> {
        default_unit_dir()
    }

    fn binary_path(&self) -> Result<PathBuf> {
        binary_path()
    }

    #[allow(unused_variables)]
    fn activate(&self, unit_path: &Path) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut reload = Command::new("systemctl");
            reload.args(["--user", "daemon-reload"]);
            if !run_step("activate", "daemon-reload", reload).is_success() {
                return false;
            }
            let mut enable = Command::new("systemctl");
            enable.args(["--user", "enable", "--now", SERVICE_FILENAME]);
            run_step("activate", "enable-now", enable).is_success()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    #[allow(unused_variables)]
    fn deactivate(&self, unit_path: &Path) {
        #[cfg(target_os = "linux")]
        {
            let mut disable = Command::new("systemctl");
            disable.args(["--user", "disable", "--now", SERVICE_FILENAME]);
            let _ = run_step("deactivate", "disable-now", disable);
        }
        #[cfg(target_os = "macos")]
        {
            let mut unload = Command::new("launchctl");
            unload.args(["unload", unit_path.to_string_lossy().as_ref()]);
            let _ = run_step("deactivate", "launchctl-unload", unload);
        }
        #[cfg(target_os = "windows")]
        {
            let mut delete = Command::new("schtasks");
            delete.args(["/Delete", "/TN", "MinisStudioWorker", "/F"]);
            let _ = run_step("deactivate", "schtasks-delete", delete);
        }
    }
}

pub fn install(config_path: Option<&str>) -> Result<()> {
    install_with(&RealOps, config_path)
}

pub fn uninstall() -> Result<()> {
    uninstall_with(&RealOps)
}

/// Write the unit file using the supplied ops and print manual activation
/// instructions if the platform tool isn't available.  Public-but-`pub`
/// so tests in `tests/` can drive it with a fake ops.
pub fn install_with<O: ServiceOps>(ops: &O, config_path: Option<&str>) -> Result<()> {
    let bin = ops.binary_path()?;
    let cfg_arg = config_path
        .map(|p| format!("--config {p} "))
        .unwrap_or_default();
    let dir = ops.unit_dir()?;
    let path = dir.join(SERVICE_FILENAME);

    let body = render_service(&bin.display().to_string(), &cfg_arg);
    std::fs::write(&path, &body)
        .with_context(|| format!("writing service file {}", path.display()))?;

    println!("wrote service unit: {}", path.display());

    let activated = ops.activate(&path);
    if activated {
        println!("activated service unit");
    } else {
        print_activation_instructions(&path);
    }
    info!(
        target: TRACE_TARGET,
        op = "install",
        unit_path = %path.display(),
        binary_path = %bin.display(),
        activated,
        "service install completed"
    );
    Ok(())
}

pub fn uninstall_with<O: ServiceOps>(ops: &O) -> Result<()> {
    let dir = ops.unit_dir()?;
    let path = dir.join(SERVICE_FILENAME);
    ops.deactivate(&path);
    let removed = if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed service unit: {}", path.display());
        true
    } else {
        println!("no service unit to remove at {}", path.display());
        false
    };
    info!(
        target: TRACE_TARGET,
        op = "uninstall",
        unit_path = %path.display(),
        removed,
        "service uninstall completed"
    );
    Ok(())
}

fn print_activation_instructions(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        println!("activate manually:");
        println!("  systemctl --user daemon-reload");
        println!("  systemctl --user enable --now {SERVICE_FILENAME}");
        let _ = path;
    }
    #[cfg(target_os = "macos")]
    println!("load with: launchctl load -w {}", path.display());
    #[cfg(target_os = "windows")]
    println!(
        "register with: schtasks /Create /XML {} /TN MinisStudioWorker",
        path.display()
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = path;
}

#[cfg(target_os = "linux")]
pub(crate) fn render_service(bin: &str, cfg_arg: &str) -> String {
    format!(
        r#"[Unit]
Description=Minis studio worker (pull-based image-generation agent)
After=network-online.target

[Service]
Type=simple
ExecStart={bin} {cfg_arg}run
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=studio_worker=info

[Install]
WantedBy=default.target
"#
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn render_service(bin: &str, cfg_arg: &str) -> String {
    let cfg_args = cfg_arg.trim();
    let extra = if cfg_args.is_empty() {
        String::new()
    } else {
        cfg_args
            .split_whitespace()
            .map(|s| format!("    <string>{}</string>\n", s))
            .collect::<String>()
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>gg.minis.studio-worker</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
{extra}    <string>run</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>EnvironmentVariables</key>
  <dict><key>RUST_LOG</key><string>studio_worker=info</string></dict>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "windows")]
pub(crate) fn render_service(bin: &str, cfg_arg: &str) -> String {
    let args = format!("{cfg_arg}run").trim().to_string();
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled></LogonTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>10</Count>
    </RestartOnFailure>
  </Settings>
  <Actions>
    <Exec>
      <Command>{bin}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tempfile::tempdir;

    struct FakeOps {
        bin: PathBuf,
        dir: PathBuf,
        activate_returns: bool,
        activate_calls: RefCell<Vec<PathBuf>>,
        deactivate_calls: RefCell<Vec<PathBuf>>,
    }

    impl ServiceOps for FakeOps {
        fn unit_dir(&self) -> Result<PathBuf> {
            Ok(self.dir.clone())
        }
        fn binary_path(&self) -> Result<PathBuf> {
            Ok(self.bin.clone())
        }
        fn activate(&self, unit_path: &Path) -> bool {
            self.activate_calls
                .borrow_mut()
                .push(unit_path.to_path_buf());
            self.activate_returns
        }
        fn deactivate(&self, unit_path: &Path) {
            self.deactivate_calls
                .borrow_mut()
                .push(unit_path.to_path_buf());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_render_includes_exec_start_and_install_section() {
        let rendered = render_service("/usr/bin/studio-worker", "");
        assert!(rendered.contains("ExecStart=/usr/bin/studio-worker run"));
        assert!(rendered.contains("[Install]"));
        assert!(rendered.contains("Restart=on-failure"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_render_passes_config_arg() {
        let rendered = render_service("/usr/bin/studio-worker", "--config /etc/conf.toml ");
        assert!(rendered.contains("--config /etc/conf.toml run"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_render_emits_valid_plist_xml() {
        let rendered = render_service("/usr/local/bin/studio-worker", "");
        assert!(rendered.contains("<plist version=\"1.0\">"));
        assert!(rendered.contains("<string>/usr/local/bin/studio-worker</string>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_render_includes_config_args_when_provided() {
        let rendered = render_service("/usr/local/bin/studio-worker", "--config /etc/conf.toml ");
        assert!(rendered.contains("<string>--config</string>"));
        assert!(rendered.contains("<string>/etc/conf.toml</string>"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_render_emits_valid_task_xml() {
        let rendered = render_service("C:\\worker.exe", "");
        assert!(rendered.contains("<Command>C:\\worker.exe</Command>"));
        assert!(rendered.contains("<Arguments>run</Arguments>"));
    }

    #[test]
    fn install_with_writes_unit_file_and_succeeds_when_activate_returns_true() {
        let dir = tempdir().unwrap();
        let ops = FakeOps {
            bin: PathBuf::from("/usr/bin/studio-worker"),
            dir: dir.path().to_path_buf(),
            activate_returns: true,
            activate_calls: RefCell::new(Vec::new()),
            deactivate_calls: RefCell::new(Vec::new()),
        };
        install_with(&ops, Some("/etc/conf.toml")).unwrap();
        let written = dir.path().join(SERVICE_FILENAME);
        assert!(
            written.exists(),
            "unit file should exist at {}",
            written.display()
        );
        let body = std::fs::read_to_string(&written).unwrap();
        assert!(body.contains("studio-worker"));
        assert_eq!(ops.activate_calls.borrow().len(), 1);
        assert_eq!(ops.activate_calls.borrow()[0], written);
    }

    #[test]
    fn install_with_falls_back_to_manual_instructions_when_activate_fails() {
        let dir = tempdir().unwrap();
        let ops = FakeOps {
            bin: PathBuf::from("/usr/bin/studio-worker"),
            dir: dir.path().to_path_buf(),
            activate_returns: false,
            activate_calls: RefCell::new(Vec::new()),
            deactivate_calls: RefCell::new(Vec::new()),
        };
        install_with(&ops, None).unwrap();
        assert!(dir.path().join(SERVICE_FILENAME).exists());
    }

    #[test]
    fn uninstall_with_removes_file_and_calls_deactivate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SERVICE_FILENAME);
        std::fs::write(&path, "dummy").unwrap();
        let ops = FakeOps {
            bin: PathBuf::from("/usr/bin/studio-worker"),
            dir: dir.path().to_path_buf(),
            activate_returns: false,
            activate_calls: RefCell::new(Vec::new()),
            deactivate_calls: RefCell::new(Vec::new()),
        };
        uninstall_with(&ops).unwrap();
        assert!(!path.exists());
        assert_eq!(ops.deactivate_calls.borrow().len(), 1);
    }

    #[test]
    fn uninstall_with_is_idempotent_when_file_missing() {
        let dir = tempdir().unwrap();
        let ops = FakeOps {
            bin: PathBuf::from("/usr/bin/studio-worker"),
            dir: dir.path().to_path_buf(),
            activate_returns: false,
            activate_calls: RefCell::new(Vec::new()),
            deactivate_calls: RefCell::new(Vec::new()),
        };
        // No file written; uninstall should still succeed.
        uninstall_with(&ops).unwrap();
    }

    // -----------------------------------------------------------------
    // Structured tracing — proves install/uninstall and the per-platform
    // RealOps steps emit operator-visible breadcrumbs.  Without these,
    // a failing `systemctl enable --now`, `launchctl unload` or
    // `schtasks /Delete` would silently leave the worker un-activated
    // (or still registered) with no log trail to diagnose it.
    // -----------------------------------------------------------------

    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct CapturingMakeWriter(Arc<Mutex<Vec<u8>>>);

    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for CapturingMakeWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturingWriter(self.0.clone())
        }
    }

    impl io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture<F: FnOnce() + Send + 'static>(f: F) -> String {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_for_thread = buf.clone();
        // Run the closure on a fresh OS thread so the thread-local
        // `with_default` is isolated from whatever subscriber state the
        // test runner left on its worker threads.  Mirrors the pattern
        // in tests/http_errors.rs and tests/auto_update.rs.
        std::thread::spawn(move || {
            let make_writer = CapturingMakeWriter(buf_for_thread);
            let subscriber = tracing_subscriber::fmt()
                .with_writer(make_writer)
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .without_time()
                .finish();
            tracing::subscriber::with_default(subscriber, f);
        })
        .join()
        .expect("capture thread panicked");
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).expect("tracing output should be valid UTF-8")
    }

    fn fake_ops(dir: PathBuf, activate_returns: bool) -> FakeOps {
        FakeOps {
            bin: PathBuf::from("/usr/bin/studio-worker"),
            dir,
            activate_returns,
            activate_calls: RefCell::new(Vec::new()),
            deactivate_calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn install_with_emits_info_event_with_activated_true_when_activation_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let logs = capture(move || {
            install_with(&fake_ops(dir_path, true), None).unwrap();
        });
        assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
        assert!(
            logs.contains("studio_worker::service"),
            "expected service target, got: {logs}"
        );
        assert!(logs.contains("op=\"install\""), "expected op field: {logs}");
        assert!(
            logs.contains("activated=true"),
            "expected activated=true: {logs}"
        );
        assert!(
            logs.contains(SERVICE_FILENAME),
            "expected unit_path in log, got: {logs}"
        );
    }

    #[test]
    fn install_with_emits_info_event_with_activated_false_on_manual_fallback() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let logs = capture(move || {
            install_with(&fake_ops(dir_path, false), None).unwrap();
        });
        assert!(
            logs.contains("activated=false"),
            "expected activated=false: {logs}"
        );
    }

    #[test]
    fn uninstall_with_emits_info_event_with_removed_true_when_file_existed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(SERVICE_FILENAME);
        std::fs::write(&path, "dummy").unwrap();
        let dir_path = dir.path().to_path_buf();
        let logs = capture(move || {
            uninstall_with(&fake_ops(dir_path, false)).unwrap();
        });
        assert!(
            logs.contains("op=\"uninstall\""),
            "expected op field: {logs}"
        );
        assert!(
            logs.contains("removed=true"),
            "expected removed=true: {logs}"
        );
    }

    #[test]
    fn uninstall_with_emits_info_event_with_removed_false_when_file_missing() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let logs = capture(move || {
            uninstall_with(&fake_ops(dir_path, false)).unwrap();
        });
        assert!(
            logs.contains("removed=false"),
            "expected removed=false: {logs}"
        );
    }

    // -----------------------------------------------------------------
    // StepOutcome — pure helper that classifies the outcome of an OS
    // command spawned by RealOps so install/uninstall never silently
    // swallow non-zero exits or missing tools.
    // -----------------------------------------------------------------

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
    fn step_outcome_is_success_only_for_succeeded() {
        assert!(StepOutcome::Succeeded.is_success());
        assert!(!StepOutcome::Failed { code: Some(1) }.is_success());
        assert!(!StepOutcome::Failed { code: None }.is_success());
        assert!(!StepOutcome::SpawnFailed.is_success());
    }
}
