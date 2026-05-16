//! Studio worker — pull-based image / LLM / audio / video generation
//! agent for minis.gg.  This file is the thinnest possible CLI entry
//! point; all logic lives in the library so it's testable.

use anyhow::Result;
use clap::Parser;
use studio_worker::{cli, run_cli};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("studio_worker=info,warn")),
        )
        .with_target(false)
        .init();

    let cli_args = cli::Cli::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run_cli(cli_args));
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    result
}
