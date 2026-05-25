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
pub mod telemetry;
#[doc(hidden)]
pub mod test_support;
pub mod types;
#[cfg(feature = "ui")]
pub mod ui;
pub mod update;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sentry release identifier in the `<pkg>@<version>` form expected by
/// Sentry's organisation-wide *Releases* feature.  Bare version strings
/// collide across projects in the same org, so we namespace with the
/// crate name.  Matches what `sentry::release_name!()` would expand to.
pub const RELEASE_NAME: &str = concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION"));

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
        cli::Command::Ui => run_ui(args.config.as_deref()).await,
    }
}

#[cfg(feature = "ui")]
async fn run_ui(config_path: Option<&str>) -> anyhow::Result<()> {
    ui::run(config_path)
}

#[cfg(not(feature = "ui"))]
async fn run_ui(_config_path: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!(
        "this build of studio-worker was compiled without the `ui` cargo feature.\n\
         Reinstall with `cargo install studio-worker --features ui` (or use the \
         desktop installer from the releases page) to enable the native UI."
    )
}
