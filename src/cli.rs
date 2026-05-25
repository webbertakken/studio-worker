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
    ///   * set the human label shown in the studio's Pending
    ///     Workers panel (`--label`)
    ///   * point the worker at a different studio (`--api-base-url`)
    ///   * clear local registration state after a rejection or
    ///     between studios (`--reset`)
    Register {
        #[arg(long)]
        api_base_url: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        reset: bool,
    },
    /// Print local config + last heartbeat info.
    Status,
    /// Install platform-appropriate auto-start service.
    InstallService,
    /// Uninstall the auto-start service.
    UninstallService,
    /// Enable auto-claim.
    Enable,
    /// Disable auto-claim.
    Disable,
    /// Set the VRAM threshold (GB) the worker reports.
    SetThreshold { gb: f32 },
    /// Print resolved config + relevant paths.
    Config,
    /// Check the release feed for a newer version (does not install).
    CheckUpdate,
    /// Launch the desktop UI (requires the `ui` cargo feature).
    Ui,
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
                label,
                reset,
            } => {
                assert_eq!(api_base_url.as_deref(), Some("https://example.invalid"));
                assert!(label.is_none());
                assert!(!reset);
            }
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn parses_register_with_label_and_reset() {
        let cli = Cli::parse_from([
            "studio-worker",
            "register",
            "--label",
            "alice's rig",
            "--reset",
        ]);
        match cli.command {
            Command::Register {
                api_base_url,
                label,
                reset,
            } => {
                assert!(api_base_url.is_none());
                assert_eq!(label.as_deref(), Some("alice's rig"));
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
                label: None,
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
    fn parses_all_simple_subcommands() {
        let cases = [
            ("status", Command::Status),
            ("install-service", Command::InstallService),
            ("uninstall-service", Command::UninstallService),
            ("enable", Command::Enable),
            ("disable", Command::Disable),
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
