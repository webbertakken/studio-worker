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

pub mod admission;

/// The log filter when `RUST_LOG` is unset: the worker's own info, llama.cpp's info (model
/// loads, `offloaded N/N layers to GPU`), and warnings from everything else. llama.cpp's debug
/// level (per-layer cache setup, `CUDA Graph id N reused` on every decode) stays out: it is
/// hundreds of lines per chat.
pub const DEFAULT_LOG_FILTER: &str = "studio_worker=info,llama-cpp-2=info,warn";
pub mod auto_register;
pub mod autostart;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod control;
pub mod daemon_api;
pub mod daemon_client;
pub mod daemon_link;
pub mod daemon_lock;
pub mod engine;
pub mod exe_watch;
pub mod glb;
pub mod host;
pub mod http;
pub mod job_gate;
pub mod job_log;
pub mod job_run;
pub mod legacy_service;
pub mod lifecycle;
pub mod loaders;
pub mod local;
pub mod local_api;
pub mod local_vision;
pub mod log_trim;
pub mod multipart;
pub mod net;
pub mod presence;
pub mod process;
pub mod residency;
pub mod runtime;
pub mod secrets;
pub mod self_install;
pub mod setup;
pub mod stt_stream;
pub mod sys;
pub mod telemetry;
#[doc(hidden)]
pub mod test_support;
pub mod thumbnail;
pub mod tray_watchdog;
pub mod types;
#[cfg(feature = "ui")]
pub mod ui;
pub mod ui_log;
pub mod update;
pub mod variant;
pub mod vision;
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

/// The file the tray UI logs to: `<config dir>/ui.log` for `ui`, nothing for the other commands
/// (and nothing when the config path cannot be resolved; the UI then reports that itself).
pub fn ui_log_target(
    command: &cli::Command,
    config_path: Option<&str>,
) -> Option<std::path::PathBuf> {
    match command {
        cli::Command::Ui => config::resolve_path(config_path)
            .ok()
            .map(|path| daemon_link::ui_log_path(&path)),
        _ => None,
    }
}

/// Dispatch table for the CLI subcommands.  Lives in the library so we
/// can drive it from tests without invoking the binary.
pub async fn run_cli(args: cli::Cli) -> anyhow::Result<()> {
    if let Some(log) = ui_log_target(&args.command, args.config.as_deref()) {
        if let Err(e) = ui_log::open(&log) {
            tracing::warn!(
                target: CLI_TRACE_TARGET,
                op = "startup",
                log = %log.display(),
                error = %e,
                "could not open the tray UI log; logging to stdout only"
            );
        }
    }
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
        cli::Command::Setup => setup::setup(args.config.as_deref()),
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
    fn only_the_tray_ui_logs_to_ui_log_beside_its_config() {
        assert_eq!(
            ui_log_target(&cli::Command::Ui, Some("/home/a/.config/sw/config.toml")),
            Some(std::path::PathBuf::from("/home/a/.config/sw/ui.log"))
        );
        assert_eq!(
            ui_log_target(&cli::Command::Run, Some("/home/a/.config/sw/config.toml")),
            None
        );
        assert_eq!(
            ui_log_target(&cli::Command::Setup, Some("/home/a/.config/sw/config.toml")),
            None
        );
    }

    #[test]
    fn the_default_log_filter_keeps_llama_cpp_info_and_drops_its_debug() {
        use tracing_subscriber::prelude::*;
        let filter = tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_FILTER).expect("parses");
        let subscriber = tracing_subscriber::registry().with(filter);
        tracing::subscriber::with_default(subscriber, || {
            // llama.cpp's load summary (`offloaded N/N layers to GPU`) is info.
            assert!(tracing::enabled!(target: "llama-cpp-2", tracing::Level::INFO));
            // Its per-decode chatter (`CUDA Graph id N reused`) is debug.
            assert!(!tracing::enabled!(target: "llama-cpp-2", tracing::Level::DEBUG));
            assert!(tracing::enabled!(target: "studio_worker", tracing::Level::INFO));
            assert!(!tracing::enabled!(target: "some_dependency", tracing::Level::INFO));
            assert!(tracing::enabled!(target: "some_dependency", tracing::Level::WARN));
        });
    }

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
