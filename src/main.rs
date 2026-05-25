//! Studio worker — pull-based image / LLM / audio / video generation
//! agent for minis.gg.  This file is the thinnest possible CLI entry
//! point; all logic lives in the library so it's testable.

use anyhow::Result;
use clap::Parser;
use studio_worker::{cli, run_cli, telemetry};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

fn main() -> Result<()> {
    // Install the tracing subscriber first so that the telemetry init
    // banner (and any warning about an invalid `SENTRY_DSN`) actually
    // surfaces.  The `sentry-tracing` layer is added now too; it uses
    // `Hub::current()` at event-emission time, so events emitted
    // before `sentry::init` simply have nowhere to go — events emitted
    // after will flow to Sentry.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("studio_worker=info,warn"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(telemetry::tracing_layer())
        .init();

    // Initialise Sentry.  When no `SENTRY_DSN` is set this is a no-op
    // and `_sentry_guard` is `None`.  Kept in a binding so the guard's
    // `Drop` flushes pending events on shutdown.
    let _sentry_guard = telemetry::init();

    let cli_args = cli::Cli::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run_cli(cli_args));
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    result
}
