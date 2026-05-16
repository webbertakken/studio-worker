//! Library surface for the `studio-worker` binary.
//!
//! Exposes the worker's modules so integration tests (and downstream
//! tooling) can drive the contract without going through the CLI.

pub mod config;
pub mod engine;
pub mod http;
pub mod runtime;
pub mod service;
pub mod sys;
pub mod types;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
