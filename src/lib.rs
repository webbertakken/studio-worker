//! Library surface for the `studio-worker` binary.
//!
//! Exposes the worker's modules so integration tests (and downstream
//! tooling) can drive the contract without going through the CLI.

// Enable the unstable `#[coverage(off)]` attribute used across the
// crate (behind `#[cfg_attr(coverage_nightly, ...)]`) to exclude
// host-, network-, and platform-dependent code from `cargo llvm-cov`.
// `coverage_nightly` is set only by cargo-llvm-cov on a nightly
// toolchain; on stable it's unset, so this gate (and every
// `coverage(off)` annotation) compiles to nothing. Without it,
// `cargo +nightly llvm-cov` fails to compile the crate (E0658).
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod auto_register;
pub mod autostart;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod engine;
pub mod http;
pub mod local;
pub mod local_api;
pub mod runtime;
pub mod secrets;
pub mod service;
pub mod sys;
pub mod telemetry;
#[doc(hidden)]
pub mod test_support;
pub mod types;
#[cfg(feature = "ui")]
pub mod ui;
pub mod update;
pub mod ws;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sentry release identifier in the `<pkg>@<version>` form expected by
/// Sentry's organisation-wide *Releases* feature.  Bare version strings
/// collide across projects in the same org, so we namespace with the
/// crate name.  Matches what `sentry::release_name!()` would expand to.
pub const RELEASE_NAME: &str = concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION"));

/// Tracing target for CLI lifecycle events.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::cli=info`.
const CLI_TRACE_TARGET: &str = "studio_worker::cli";

/// Emit a single startup breadcrumb naming the agent version and the
/// subcommand about to run.  With no `SENTRY_DSN` set (the default),
/// nothing else anchors "which version started, running what" in
/// `journalctl` — which matters most right after an auto-update
/// re-execs the binary or a service manager restarts the unit.
fn log_cli_startup(command: &cli::Command) {
    tracing::info!(
        target: CLI_TRACE_TARGET,
        op = "startup",
        version = AGENT_VERSION,
        command = command.name(),
        "studio-worker starting"
    );
}

/// Dispatch table for the CLI subcommands.  Lives in the library so we
/// can drive it from tests without invoking the binary.
pub async fn run_cli(args: cli::Cli) -> anyhow::Result<()> {
    log_cli_startup(&args.command);
    match args.command {
        cli::Command::Run => runtime::run(args.config.as_deref()).await,
        cli::Command::Register {
            api_base_url,
            reset,
        } => {
            runtime::register(
                args.config.as_deref(),
                runtime::RegisterArgs {
                    api_base_url,
                    reset,
                },
            )
            .await
        }
        cli::Command::Status => runtime::status(args.config.as_deref()).await,
        cli::Command::InstallService => service::install(args.config.as_deref()),
        cli::Command::UninstallService => service::uninstall(),
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
        "this build of studio-worker was compiled without the `ui` cargo feature \
         (it is on by default \u{2014} you built with `--no-default-features`).\n\
         Reinstall with `cargo install studio-worker` (UI is the default), or use \
         the desktop installer from the releases page, to enable the native UI."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    #[test]
    fn startup_breadcrumb_names_version_and_command() {
        let logs = capture(|| log_cli_startup(&cli::Command::Run));
        assert!(logs.contains("INFO"), "expected INFO event, got: {logs}");
        assert!(
            logs.contains("studio_worker::cli"),
            "expected the cli target, got: {logs}"
        );
        assert!(
            logs.contains("op=\"startup\""),
            "expected op=startup, got: {logs}"
        );
        assert!(
            logs.contains("command=\"run\""),
            "expected command field, got: {logs}"
        );
        assert!(
            logs.contains(AGENT_VERSION),
            "expected agent version, got: {logs}"
        );
    }
}
