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
            let status = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            if status.map(|s| s.success()).unwrap_or(false) {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "enable", "--now", SERVICE_FILENAME])
                    .status();
                return true;
            }
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn deactivate(&self, _unit_path: &Path) {
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", SERVICE_FILENAME])
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", _unit_path.to_string_lossy().as_ref()])
                .status();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("schtasks")
                .args(["/Delete", "/TN", "MinisStudioWorker", "/F"])
                .status();
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

    if ops.activate(&path) {
        println!("activated service unit");
    } else {
        print_activation_instructions(&path);
    }
    Ok(())
}

pub fn uninstall_with<O: ServiceOps>(ops: &O) -> Result<()> {
    let dir = ops.unit_dir()?;
    let path = dir.join(SERVICE_FILENAME);
    ops.deactivate(&path);
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed service unit: {}", path.display());
    } else {
        println!("no service unit to remove at {}", path.display());
    }
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
}
