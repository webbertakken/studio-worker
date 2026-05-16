//! Studio worker — pull-based image-generation agent for minis.gg.
//!
//! Subcommands:
//!   `run`               start the heartbeat + claim loop
//!   `register`          one-shot register with the API
//!   `status`            print local config + last heartbeat
//!   `install-service`   install platform-appropriate auto-start service
//!   `uninstall-service` remove the installed service
//!   `enable`/`disable`  toggle the auto-enabled flag in config
//!   `set-threshold N`   change the VRAM threshold (GB)
//!   `config`            print resolved config + paths

use anyhow::Result;
use clap::{Parser, Subcommand};
use studio_worker::{runtime, service};
use tracing::error;

#[derive(Parser, Debug)]
#[command(
    name = "studio-worker",
    version,
    about = "Studio worker — pull-based image-generation agent"
)]
struct Cli {
    /// Override the path to config.toml.
    #[arg(long, global = true)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the heartbeat + claim loop.
    Run,
    /// Register the worker against the API (idempotent).
    Register {
        #[arg(long)]
        bootstrap_token: Option<String>,
        #[arg(long)]
        api_base_url: Option<String>,
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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("studio_worker=info,warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let runtime_handle = tokio::runtime::Runtime::new()?;

    let result = runtime_handle.block_on(async {
        match cli.command {
            Command::Run => runtime::run(cli.config.as_deref()).await,
            Command::Register {
                bootstrap_token,
                api_base_url,
            } => runtime::register(cli.config.as_deref(), bootstrap_token, api_base_url).await,
            Command::Status => runtime::status(cli.config.as_deref()).await,
            Command::InstallService => service::install(cli.config.as_deref()),
            Command::UninstallService => service::uninstall(),
            Command::Enable => runtime::set_enabled(cli.config.as_deref(), true),
            Command::Disable => runtime::set_enabled(cli.config.as_deref(), false),
            Command::SetThreshold { gb } => runtime::set_threshold(cli.config.as_deref(), gb),
            Command::Config => runtime::show_config(cli.config.as_deref()),
        }
    });

    if let Err(e) = &result {
        error!("{:#}", e);
    }
    result
}
