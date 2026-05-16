//! Library surface for the `studio-worker` binary.
//!
//! Exposes the worker's modules so integration tests (and downstream
//! tooling) can drive the contract without going through the CLI.

pub mod cli;
pub mod config;
pub mod engine;
pub mod http;
pub mod runtime;
pub mod service;
pub mod sys;
pub mod types;
pub mod update;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Dispatch table for the CLI subcommands.  Lives in the library so we
/// can drive it from tests without invoking the binary.
pub async fn run_cli(args: cli::Cli) -> anyhow::Result<()> {
    match args.command {
        cli::Command::Run => runtime::run(args.config.as_deref()).await,
        cli::Command::Register {
            bootstrap_token,
            api_base_url,
        } => runtime::register(args.config.as_deref(), bootstrap_token, api_base_url).await,
        cli::Command::Status => runtime::status(args.config.as_deref()).await,
        cli::Command::InstallService => service::install(args.config.as_deref()),
        cli::Command::UninstallService => service::uninstall(),
        cli::Command::Enable => runtime::set_enabled(args.config.as_deref(), true),
        cli::Command::Disable => runtime::set_enabled(args.config.as_deref(), false),
        cli::Command::SetThreshold { gb } => runtime::set_threshold(args.config.as_deref(), gb),
        cli::Command::Config => runtime::show_config(args.config.as_deref()),
        cli::Command::CheckUpdate => runtime::check_update(args.config.as_deref()).await,
    }
}
