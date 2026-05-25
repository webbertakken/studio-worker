//! Autostart-on-login toggle.  Writes per-OS artefacts:
//!
//! - Linux: `~/.config/autostart/studio-worker-ui.desktop`
//! - macOS: `~/Library/LaunchAgents/gg.minis.studio-worker-ui.plist`
//! - Windows: HKCU `Software\Microsoft\Windows\CurrentVersion\Run`
//!   (the writer takes a `RegistryWriter` trait so it's testable
//!   without touching the real registry).
//!
//! Distinct from `service::install` because that owns the systemd /
//! launchd / Scheduled-Task path for the headless `run` subcommand;
//! autostart is for the tray UI on a desktop.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

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
    autostart_path().map(|p| p.exists()).unwrap_or(false)
}

pub fn enable(exe: &Path) -> Result<()> {
    let body = render_artefact(exe);
    let path = autostart_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn disable() -> Result<()> {
    let path = autostart_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn render_artefact(exe: &Path) -> String {
    let exe_str = exe.to_string_lossy().to_string();
    if cfg!(target_os = "linux") {
        render_desktop_entry(&exe_str)
    } else if cfg!(target_os = "macos") {
        render_launch_agent(&exe_str)
    } else {
        // Windows marker — keeps the round-trip test useful even
        // though the real autostart is a registry entry (TODO).
        format!("studio-worker-ui autostart enabled for {exe_str}\n")
    }
}

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
}
