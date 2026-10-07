//! `studio-worker setup`: finish an install.  The installers run it after installing the binary
//! (not when the auto-updater runs them): it installs the tray UI's login entry, starts the
//! tray UI detached, and prints what to do next.  Installed, the worker always runs as the tray
//! UI.  See `docs/runtime/daemon-and-tray.md#install`.
//!
//! The side effects go through [`SetupOps`] so the flow is unit-tested without them.
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const TRACE_TARGET: &str = "studio_worker::setup";

/// The side effects of `setup`.
pub trait SetupOps {
    /// The executable to start and to point the login entry at.
    fn exe(&self) -> Result<PathBuf>;
    /// Install (or refresh) the tray UI's login entry when `enabled`, remove it otherwise.
    fn sync_login_entry(&self, exe: &Path, enabled: bool) -> Result<()>;
    /// Start `exe args` detached, output appended to `log`; its pid.
    fn start_ui(&self, exe: &Path, args: &[OsString], log: &Path) -> std::io::Result<u32>;
}

/// The arguments that start the tray UI for `config_path`.
pub fn ui_args(config_path: Option<&str>) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(path) = config_path {
        args.push("--config".into());
        args.push(path.into());
    }
    args.push("ui".into());
    args
}

/// What `setup` prints once the tray UI is starting.  `machine_name` is what the studio admin
/// approves; `discovery_path` is where local clients read the API's URL and token.
/// `auto_start` says whether it starts again at every login.
pub fn setup_summary(
    machine_name: &str,
    api_base_url: &str,
    discovery_path: &str,
    auto_start: bool,
) -> String {
    let base = api_base_url.trim_end_matches('/');
    let login = if auto_start {
        "It starts again at every login."
    } else {
        "It does not start at login (\"Start with my machine\" is off in Config)."
    };
    format!(
        "\nstudio-worker is installed and its tray UI is starting: look for its icon in the \
         system tray.\n{login}\n\n\
         Next step: approve this worker in the studio\n\
         \u{2022} open {base}/graphics and find this machine in the workers list\n\
         \u{2022} it appears as: {machine_name}\n\
         \u{2022} once an admin approves it, the worker starts claiming jobs automatically\n\n\
         Local image API (no studio needed):\n\
         \u{2022} URL + bearer token are written to: {discovery_path}\n\
         \u{2022} POST /image there to generate locally\n\n\
         Nothing else to do: models and GPU runtimes download on demand.\n"
    )
}

/// `setup` through `ops`.
pub fn setup_with<O: SetupOps>(ops: &O, config_path: Option<&str>) -> Result<()> {
    let (cfg, path) = crate::config::load(config_path)?;
    let exe = ops.exe()?;
    if let Err(e) = ops.sync_login_entry(&exe, cfg.auto_start) {
        // The tray UI syncs it again at every start; say so and carry on.
        warn!(
            target: TRACE_TARGET,
            op = "setup",
            auto_start = cfg.auto_start,
            error = %e,
            "could not update the tray UI's login entry; the tray UI retries when it starts"
        );
    }
    let log = crate::daemon_link::ui_log_path(&path);
    let pid = ops
        .start_ui(&exe, &ui_args(config_path), &log)
        .with_context(|| format!("starting the tray UI ({} ui)", exe.display()))?;
    info!(
        target: TRACE_TARGET,
        op = "setup",
        pid,
        exe = %exe.display(),
        config_path = %path.display(),
        log = %log.display(),
        "setup started the tray UI"
    );
    let discovery = crate::config::local_api_discovery_path_for(&path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<config dir>/local-api.json".to_string());
    print!(
        "{}",
        setup_summary(
            &crate::sys::machine_name(),
            &cfg.api_base_url,
            &discovery,
            cfg.auto_start,
        )
    );
    Ok(())
}

/// `studio-worker setup`.
#[cfg(feature = "ui")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn setup(config_path: Option<&str>) -> Result<()> {
    setup_with(&RealOps, config_path)
}

/// `studio-worker setup` in a build without the tray UI: there is nothing to start.
#[cfg(not(feature = "ui"))]
pub fn setup(_config_path: Option<&str>) -> Result<()> {
    anyhow::bail!(
        "this build has no tray UI (built with `--no-default-features`), and installed the \
         worker runs only as the tray UI; install the release or `cargo install studio-worker`"
    )
}

/// The real login entry and process start.
pub struct RealOps;

impl SetupOps for RealOps {
    fn exe(&self) -> Result<PathBuf> {
        std::env::current_exe().context("resolving the current executable")
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn sync_login_entry(&self, exe: &Path, enabled: bool) -> Result<()> {
        crate::autostart::sync(exe, enabled)
    }

    // Starts a real, detached tray UI; exercised by the installer runs, not unit tests.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn start_ui(&self, exe: &Path, args: &[OsString], log: &Path) -> std::io::Result<u32> {
        use std::process::{Command, Stdio};
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out);
        // Its own process group / no console, so it outlives the installer.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        Ok(cmd.spawn()?.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[derive(Default)]
    struct FakeOps {
        login_entry_fails: bool,
        start_fails: bool,
        login_entries: Mutex<Vec<(PathBuf, bool)>>,
        started: Mutex<Vec<(PathBuf, Vec<OsString>, PathBuf)>>,
    }

    impl SetupOps for FakeOps {
        fn exe(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/opt/sw/studio-worker"))
        }
        fn sync_login_entry(&self, exe: &Path, enabled: bool) -> Result<()> {
            self.login_entries
                .lock()
                .unwrap()
                .push((exe.to_path_buf(), enabled));
            if self.login_entry_fails {
                anyhow::bail!("no autostart dir");
            }
            Ok(())
        }
        fn start_ui(&self, exe: &Path, args: &[OsString], log: &Path) -> std::io::Result<u32> {
            self.started.lock().unwrap().push((
                exe.to_path_buf(),
                args.to_vec(),
                log.to_path_buf(),
            ));
            if self.start_fails {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
            }
            Ok(4242)
        }
    }

    fn config_in(dir: &Path) -> String {
        let path = dir.join("config.toml");
        crate::config::save(&crate::config::Config::default(), &path).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_ui_starts_with_the_config_it_was_set_up_for() {
        assert_eq!(ui_args(None), [OsString::from("ui")]);
        assert_eq!(
            ui_args(Some("/c/config.toml")),
            ["--config", "/c/config.toml", "ui"].map(OsString::from)
        );
    }

    #[test]
    fn setup_installs_the_login_entry_and_starts_the_tray_ui() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let ops = std::sync::Arc::new(FakeOps::default());
        let in_capture = ops.clone();
        let logs = capture(move || setup_with(&*in_capture, Some(&config)).unwrap());
        assert_eq!(
            *ops.login_entries.lock().unwrap(),
            [(PathBuf::from("/opt/sw/studio-worker"), true)]
        );
        let started = ops.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        let (exe, args, log) = &started[0];
        assert_eq!(exe, Path::new("/opt/sw/studio-worker"));
        assert_eq!(args.last().unwrap(), "ui");
        assert_eq!(log, &dir.path().join("ui.log"));
        assert!(
            logs.contains("op=\"setup\"") || logs.contains("op=setup"),
            "{logs}"
        );
        assert!(logs.contains("setup started the tray UI"), "{logs}");
        assert!(logs.contains("pid=4242"), "{logs}");
    }

    #[test]
    fn a_failed_login_entry_warns_and_the_ui_still_starts() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let ops = std::sync::Arc::new(FakeOps {
            login_entry_fails: true,
            ..FakeOps::default()
        });
        let in_capture = ops.clone();
        let logs = capture(move || setup_with(&*in_capture, Some(&config)).unwrap());
        assert!(
            logs.contains("WARN") && logs.contains("login entry"),
            "{logs}"
        );
        assert_eq!(ops.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_ui_that_cannot_start_fails_setup_with_the_command() {
        let dir = tempdir().unwrap();
        let config = config_in(dir.path());
        let ops = FakeOps {
            start_fails: true,
            ..FakeOps::default()
        };
        let err = format!("{:#}", setup_with(&ops, Some(&config)).unwrap_err());
        assert!(err.contains("starting the tray UI"), "{err}");
        assert!(err.contains("/opt/sw/studio-worker ui"), "{err}");
    }

    #[test]
    fn setup_summary_names_machine_studio_and_discovery() {
        let s = setup_summary(
            "alices-rig",
            "https://studio.minis.gg/",
            "/home/alice/.config/minis-studio-worker/local-api.json",
            true,
        );
        assert!(s.contains("alices-rig"), "must name the machine: {s}");
        assert!(s.contains("https://studio.minis.gg/graphics"), "got: {s}");
        assert!(
            !s.contains(".gg//graphics"),
            "trailing slash not trimmed: {s}"
        );
        assert!(
            s.contains("local-api.json"),
            "must point at discovery file: {s}"
        );
        assert!(s.contains("tray UI is starting"), "got: {s}");
        assert!(s.contains("download on demand"), "got: {s}");
        assert!(s.contains("every login"), "got: {s}");
        let off = setup_summary("alices-rig", "https://studio.minis.gg/", "x", false);
        assert!(off.contains("does not start at login"), "got: {off}");
    }

    #[test]
    fn setup_removes_the_login_entry_when_auto_start_is_off() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = crate::config::Config {
            auto_start: false,
            ..crate::config::Config::default()
        };
        crate::config::save(&cfg, &path).unwrap();
        let ops = FakeOps::default();
        setup_with(&ops, Some(&path.to_string_lossy())).unwrap();
        assert_eq!(
            *ops.login_entries.lock().unwrap(),
            [(PathBuf::from("/opt/sw/studio-worker"), false)]
        );
        assert_eq!(ops.started.lock().unwrap().len(), 1);
    }

    #[cfg(not(feature = "ui"))]
    #[test]
    fn a_build_without_the_tray_ui_refuses_setup() {
        let err = setup(None).unwrap_err().to_string();
        assert!(err.contains("no tray UI"), "{err}");
    }
}
