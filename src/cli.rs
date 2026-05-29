//! Clap CLI definitions, kept out of `main.rs` so they're testable.
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "studio-worker",
    version,
    about = "Studio worker — pull-based generation agent (image / llm / audio / video)"
)]
pub struct Cli {
    /// Override the path to config.toml.
    #[arg(long, global = true)]
    pub config: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum Command {
    /// Start the heartbeat + claim loop.
    Run,
    /// Pre-set registration metadata before the next launch.
    ///
    /// On a fresh install you don't need this — `run` and `ui`
    /// auto-register themselves.  Use it explicitly to:
    ///   * point the worker at a different studio (`--api-base-url`)
    ///   * clear local registration state after a rejection or
    ///     between studios (`--reset`)
    Register {
        #[arg(long)]
        api_base_url: Option<String>,
        #[arg(long)]
        reset: bool,
    },
    /// Print local config + last heartbeat info.
    Status,
    /// Install platform-appropriate auto-start service.
    InstallService,
    /// Uninstall the auto-start service.
    UninstallService,
    /// Set the VRAM threshold (GB) the worker reports.
    SetThreshold { gb: f32 },
    /// Print resolved config + relevant paths.
    Config,
    /// Check the release feed for a newer version (does not install).
    CheckUpdate,
    /// Launch the desktop UI (requires the `ui` cargo feature).
    Ui,
}

impl Command {
    /// Stable kebab-case label for the subcommand.  Used as the
    /// structured `command` field in the CLI startup breadcrumb so
    /// operators can filter `journalctl` by which subcommand a
    /// process is running.  Matches clap's derived subcommand names.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Run => "run",
            Command::Register { .. } => "register",
            Command::Status => "status",
            Command::InstallService => "install-service",
            Command::UninstallService => "uninstall-service",
            Command::SetThreshold { .. } => "set-threshold",
            Command::Config => "config",
            Command::CheckUpdate => "check-update",
            Command::Ui => "ui",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_run() {
        let cli = Cli::parse_from(["studio-worker", "run"]);
        assert!(matches!(cli.command, Command::Run));
        assert!(cli.config.is_none());
    }

    #[test]
    fn parses_run_with_config_override() {
        let cli = Cli::parse_from(["studio-worker", "--config", "/etc/x.toml", "run"]);
        assert_eq!(cli.config.as_deref(), Some("/etc/x.toml"));
        assert!(matches!(cli.command, Command::Run));
    }

    #[test]
    fn parses_register_with_overrides() {
        let cli = Cli::parse_from([
            "studio-worker",
            "register",
            "--api-base-url",
            "https://example.invalid",
        ]);
        match cli.command {
            Command::Register {
                api_base_url,
                reset,
            } => {
                assert_eq!(api_base_url.as_deref(), Some("https://example.invalid"));
                assert!(!reset);
            }
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn parses_register_with_reset() {
        let cli = Cli::parse_from(["studio-worker", "register", "--reset"]);
        match cli.command {
            Command::Register {
                api_base_url,
                reset,
            } => {
                assert!(api_base_url.is_none());
                assert!(reset);
            }
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn parses_bare_register() {
        let cli = Cli::parse_from(["studio-worker", "register"]);
        assert!(matches!(
            cli.command,
            Command::Register {
                api_base_url: None,
                reset: false,
            }
        ));
    }

    #[test]
    fn parses_set_threshold_with_float() {
        let cli = Cli::parse_from(["studio-worker", "set-threshold", "12.5"]);
        match cli.command {
            Command::SetThreshold { gb } => assert!((gb - 12.5).abs() < 1e-6),
            other => panic!("expected set-threshold, got {other:?}"),
        }
    }

    #[test]
    fn name_is_stable_kebab_case_for_every_subcommand() {
        assert_eq!(Command::Run.name(), "run");
        assert_eq!(
            Command::Register {
                api_base_url: None,
                reset: false
            }
            .name(),
            "register"
        );
        assert_eq!(Command::Status.name(), "status");
        assert_eq!(Command::InstallService.name(), "install-service");
        assert_eq!(Command::UninstallService.name(), "uninstall-service");
        assert_eq!(Command::SetThreshold { gb: 1.0 }.name(), "set-threshold");
        assert_eq!(Command::Config.name(), "config");
        assert_eq!(Command::CheckUpdate.name(), "check-update");
        assert_eq!(Command::Ui.name(), "ui");
    }

    #[test]
    fn parses_all_simple_subcommands() {
        let cases = [
            ("status", Command::Status),
            ("install-service", Command::InstallService),
            ("uninstall-service", Command::UninstallService),
            ("config", Command::Config),
            ("check-update", Command::CheckUpdate),
            ("ui", Command::Ui),
        ];
        for (name, expected) in cases {
            let cli = Cli::parse_from(["studio-worker", name]);
            assert_eq!(cli.command, expected, "parsing `{name}`");
        }
    }
}
