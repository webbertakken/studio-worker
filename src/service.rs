//! OS service install/uninstall.  Linux: systemd --user.  macOS: launchd
//! plist template (written but not loaded — operator runs `launchctl load`).
//! Windows: schtasks template (written but not registered — operator runs
//! the printed command).
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

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
fn unit_dir() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().ok_or_else(|| anyhow!("cannot resolve user dirs"))?;
    let path = dirs.config_dir().join("systemd").join("user");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn unit_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home).join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(target_os = "windows")]
fn unit_dir() -> Result<PathBuf> {
    let app_data = std::env::var("APPDATA").context("APPDATA not set")?;
    let path = PathBuf::from(app_data).join("minis-studio-worker");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn install(config_path: Option<&str>) -> Result<()> {
    let bin = binary_path()?;
    let cfg_arg = config_path
        .map(|p| format!("--config {p} "))
        .unwrap_or_default();
    let dir = unit_dir()?;
    let path = dir.join(SERVICE_FILENAME);

    let body = render_service(&bin.display().to_string(), &cfg_arg);
    std::fs::write(&path, &body)
        .with_context(|| format!("writing service file {}", path.display()))?;

    println!("wrote service unit: {}", path.display());

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        match status {
            Ok(s) if s.success() => {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "enable", "--now", SERVICE_FILENAME])
                    .status();
                println!("enabled + started systemd --user unit");
            }
            _ => {
                println!("systemd not available; reload + enable manually:");
                println!("  systemctl --user daemon-reload");
                println!("  systemctl --user enable --now {SERVICE_FILENAME}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    println!("load with: launchctl load -w {}", path.display());

    #[cfg(target_os = "windows")]
    println!(
        "register with: schtasks /Create /XML {} /TN MinisStudioWorker",
        path.display()
    );

    Ok(())
}

pub fn uninstall() -> Result<()> {
    let dir = unit_dir()?;
    let path = dir.join(SERVICE_FILENAME);

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", SERVICE_FILENAME])
            .status();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", path.to_string_lossy().as_ref()])
            .status();
    }

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", "MinisStudioWorker", "/F"])
            .status();
    }

    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed service unit: {}", path.display());
    } else {
        println!("no service unit to remove at {}", path.display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn render_service(bin: &str, cfg_arg: &str) -> String {
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
fn render_service(bin: &str, cfg_arg: &str) -> String {
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
fn render_service(bin: &str, cfg_arg: &str) -> String {
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
